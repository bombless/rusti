// Copyright 2014-2015 Rusti Project
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Parsing REPL input statements, including Rust code and `rusti` commands.

use std::borrow::Cow;
use std::borrow::Cow::*;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::mem::swap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::thread::Builder;

use rustc;

use syntax::ast::Decl_::*;
use syntax::ast::Item_::*;
use syntax::ast::MacStmtStyle::*;
use syntax::ast::Stmt_::*;
use syntax::codemap::{BytePos, CodeMap, Span};
use syntax::diagnostic::{Auto, Emitter, EmitterWriter};
use syntax::diagnostic::{Handler, Level, RenderSpan};
use syntax::diagnostic::Level::*;
use syntax::diagnostics::registry::Registry;
use syntax::parse::{classify, token, PResult};
use syntax::parse::{filemap_to_parser, ParseSess};
use syntax::parse::attr::ParserAttr;

use repl::{lookup_command, CmdArgs};

use self::InputResult::*;

use std::io::{stdout, stdin, Write};

pub struct FileReader {
    reader: BufReader<File>,
    path: PathBuf,
    buffer: String,
}

impl FileReader {
    pub fn new(f: File, path: PathBuf) -> FileReader {
        FileReader{
            reader: BufReader::new(f),
            path: path,
            buffer: String::new(),
        }
    }

    pub fn read_input(&mut self) -> InputResult {
        let mut buf = String::new();

        loop {
            let mut line = String::new();

            match self.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => (),
                Err(e) => return InputError(Some(Owned(format!("{}", e)))),
            };

            if is_command(&line) {
                if buf.is_empty() {
                    return parse_command(&line, true);
                } else {
                    self.buffer = line;
                    break;
                }
            } else {
                buf.push_str(&line);
            }
        }

        if !buf.is_empty() {
            parse_program(&buf, false,
                self.path.as_os_str().to_str())
        } else {
            Eof
        }
    }

    fn read_line(&mut self, buf: &mut String) -> io::Result<usize> {
        if self.buffer.is_empty() {
            self.reader.read_line(buf)
        } else {
            swap(buf, &mut self.buffer);
            Ok(buf.len())
        }
    }
}

/// Reads input from `stdin`
pub struct InputReader {
    buffer: String,
}

impl InputReader {
    /// Constructs a new `InputReader` reading from `stdin`.
    pub fn new() -> InputReader {
        InputReader{
            buffer: String::new(),
        }
    }

    /// Reads a single command, item, or statement from `stdin`.
    /// Returns `More` if further input is required for a complete result.
    /// In this case, the input received so far is buffered internally.
    pub fn read_input(&mut self, prompt: &str) -> InputResult {
        stdout().write(prompt.as_bytes()).unwrap();
        stdout().flush().unwrap();
        let mut line = String::new();
        if stdin().read_line(&mut line).is_err() {
            return Eof
        }

        self.buffer.push_str(&line);

        if self.buffer.is_empty() {
            return Empty;
        }

        let res = if is_command(&self.buffer) {
            parse_command(&self.buffer, true)
        } else {
            self.buffer.push('\n');
            parse_program(&self.buffer, true, None)
        };

        match res {
            More => (),
            _ => self.buffer.clear(),
        };

        res
    }

    /// Reads a block of input until receiving a line consisting only of `.`,
    /// which will return input, or `.q`, which will cancel and return `Empty`.
    ///
    /// # Panics
    ///
    /// If the internal buffer contains any data; i.e. if the last
    /// result from a call to `read_input` returned `More`.
    pub fn read_block_input(&mut self, prompt: &str) -> InputResult {
        assert!(self.buffer.is_empty());

        let mut buf = String::new();

        let mut line = String::new();

        loop {
            stdout().write(prompt.as_bytes()).unwrap();
            stdout().flush().unwrap();
            if stdin().read_line(&mut line).is_err() {
                return Eof
            }

            if line == ".q" || line == ":q" {
                return Empty;
            } else if line == "." {
                return parse_program(&buf, true, None);
            }

            buf.push_str(&line);
            buf.push('\n');
        }
    }
}

/// Possible results from reading input from `InputReader`
#[derive(Clone, Debug)]
pub enum InputResult {
    /// rusti command as input; (name, rest of line)
    Command(String, Option<String>),
    /// Code as input
    Program(Input),
    /// An empty line
    Empty,
    /// Needs more input; i.e. there is an unclosed delimiter
    More,
    /// End of file reached
    Eof,
    /// Error while parsing input; a Rust parsing error will have printed out
    /// error messages and therefore contain no error message.
    InputError(Option<Cow<'static, str>>),
}

/// Represents an input program
#[derive(Clone, Debug)]
pub struct Input {
    /// Module attributes
    pub attributes: Vec<String>,
    /// Module-level view items (`use`, `extern crate`)
    pub view_items: Vec<String>,
    /// Module-level items (`fn`, `enum`, `type`, `struct`, etc.)
    pub items: Vec<String>,
    /// Inner statements and declarations
    pub statements: Vec<String>,
    /// Whether the final statement (if there are any) is an expression
    /// without a trailing semicolon
    pub last_expr: bool,
}

impl Input {
    pub fn new() -> Input {
        Input{
            attributes: Vec::new(),
            view_items: Vec::new(),
            items: Vec::new(),
            statements: Vec::new(),
            last_expr: false,
        }
    }
}

pub fn is_command(line: &str) -> bool {
    (line.starts_with(".") && !line.starts_with("..")) ||
        (line.starts_with(":") && !line.starts_with("::"))
}

/// Parses a line of input as a command.
/// Returns either a `Command` value or an `InputError` value.
pub fn parse_command(line: &str, filter: bool) -> InputResult {
    debug!("parse_command: {:?}", line);
    if !is_command(line) {
        return InputError(Some(Borrowed("command must begin with `.` or `:`")));
    }

    let line = &line[1..];
    let mut words = line.trim_right().splitn(2, ' ');

    let name = match words.next() {
        Some(name) if !name.is_empty() => name,
        _ => return InputError(Some(Borrowed("expected command name"))),
    };

    let cmd = match lookup_command(name) {
        Some(cmd) => cmd,
        None => return InputError(Some(Owned(
            format!("unrecognized command: {}", name))))
    };

    let args = words.next();

    match cmd.accepts {
        CmdArgs::Nothing if args.is_some() => InputError(
            Some(Owned(format!("command `{}` takes no arguments", cmd.name)))),
        CmdArgs::Expr if args.is_none() => InputError(
            Some(Owned(format!("command `{}` expects an expression", cmd.name)))),
        CmdArgs::Expr => {
            let args = args.unwrap();
            match parse_program(args, filter, None) {
                Program(_) => Command(name.to_string(), Some(args.to_string())),
                i => i,
            }
        }
        _ => Command(name.to_string(), args.map(|s| s.to_string()))
    }
}

/// Parses a line of input as a program.
///
/// If there are parse errors, they will be printed to `stderr`.
/// If `filter` is true, certain errors that indicate an incomplete input
/// will result in a value of `More`. Otherwise, these errors will be emitted
/// and `InputError` will be returned.
pub fn parse_program(code: &str, filter: bool, filename: Option<&str>) -> InputResult {
    let (tx, rx) = channel();
    let (err_tx, err_rx) = channel();

    let task = Builder::new().name("parse_program".to_string());

    // Items are not returned in data structures; nor are they converted back
    // into strings. Instead, to preserve user input formatting, we use
    // byte offsets to return the input as it was received.
    fn slice(s: &String, lo: BytePos, hi: BytePos) -> String {
        s[lo.0 as usize .. hi.0 as usize].to_string()
    }

    let code = code.to_string();
    let filename = filename.unwrap_or("<input>").to_string();

    let handle = task.spawn(move || {
        if !log_enabled!(::log::LogLevel::Debug) {
            io::set_panic(Box::new(io::sink()));
        }
        let mut input = Input::new();
        let handler = Handler::with_emitter(false,
            Box::new(ErrorEmitter::new(err_tx, filter)));
        let mut sess = ParseSess::new();

        sess.span_diagnostic.handler = handler;

        let mut p = filemap_to_parser(&sess,
            sess.codemap().new_filemap(filename, code.to_string()),
            Vec::new());

        // Whether the last statement is an expression without a semicolon
        let mut last_expr = false;

        while p.token != token::Eof {
            if let token::DocComment(_) = p.token {
                try_fatal(p.bump());
                continue;
            }

            let lo = p.span.lo;

            if p.token == token::Pound {
                if p.look_ahead(1, |t| *t == token::Not) {
                    let _ = p.parse_attribute(true);
                    input.attributes.push(slice(&code, lo, p.last_span.hi));
                    continue;
                }
            }

            let stmt = match p.parse_stmt() {
                None => break,
                Some(stmt) => stmt,
            };

            let mut hi = None;

            last_expr = match stmt.node {
                StmtExpr(ref e, _) => {
                    if classify::expr_requires_semi_to_be_stmt(&**e) {
                        try_fatal(p.commit_stmt(&[], &[token::Semi, token::Eof]));
                    }
                    !try_fatal(p.eat(&token::Semi))
                }
                StmtMac(_, MacStmtWithoutBraces) => {
                    try_fatal(p.expect_one_of(&[], &[token::Semi, token::Eof]));
                    !try_fatal(p.eat(&token::Semi))
                }
                StmtMac(_, _) => false,
                StmtDecl(ref decl, _) => {
                    if let DeclLocal(_) = decl.node {
                        try_fatal(p.expect(&token::Semi));
                    } else {
                        // Consume the semicolon if there is one,
                        // but don't add it to the item
                        hi = Some(p.last_span.hi);
                        try_fatal(p.eat(&token::Semi));
                    }
                    false
                }
                _ => false
            };

            let dest = match stmt.node {
                StmtDecl(ref decl, _) => {
                    match decl.node {
                        DeclLocal(..) => &mut input.statements,
                        DeclItem(ref item) => {
                            match item.node {
                                ItemExternCrate(..) | ItemUse(..) =>
                                    &mut input.view_items,
                                _ => &mut input.items,
                            }
                        }
                    }
                },
                StmtMac(_, MacStmtWithBraces) => &mut input.items,
                _ => &mut input.statements,
            };

            dest.push(slice(&code, lo, hi.unwrap_or(p.last_span.hi)));
        }

        input.last_expr = last_expr;

        tx.send(input).unwrap();
    }).unwrap();

    match handle.join() {
        Ok(_) => {
            Program(rx.recv().unwrap())
        }
        Err(_) => {
            if err_rx.iter().any(|fatal| fatal) {
                InputError(None)
            } else {
                More
            }
        }
    }
}

fn try_fatal<T>(r: PResult<T>) -> T {
    match r {
        Ok(t) => t,
        Err(_) => panic!("fatal error")
    }
}

/// Filters error messages and reports to a channel
struct ErrorEmitter {
    /// Sends true for fatal errors; false for `More` errors
    errors: Sender<bool>,
    emitter: EmitterWriter,
    filter: bool,
}

impl ErrorEmitter {
    /// Constructs a new `ErrorEmitter` which will report fatal-ness of errors
    /// to the given channel and emit non-fatal error messages to `stderr`.
    /// If `filter` is false, all errors are considered fatal.
    fn new(tx: Sender<bool>, filter: bool) -> ErrorEmitter {
        ErrorEmitter{
            errors: tx,
            emitter: EmitterWriter::stderr(Auto,
                Some(Registry::new(&rustc::DIAGNOSTICS))),
            filter: filter,
        }
    }
}

impl Emitter for ErrorEmitter {
    fn emit(&mut self, cmsp: Option<(&CodeMap, Span)>, msg: &str,
            code: Option<&str>, lvl: Level) {
        if !self.filter {
            self.emitter.emit(cmsp, msg, code, lvl);
            self.errors.send(true).unwrap();
            return;
        }

        match lvl {
            Bug | Fatal | Error => {
                if msg.contains("un-closed delimiter") ||
                        msg.contains("expected item after attributes") ||
                        msg.contains("unterminated block comment") ||
                        msg.contains("unterminated block doc-comment") ||
                        msg.contains("unterminated double quote string") ||
                        msg.contains("unterminated double quote byte string") ||
                        msg.contains("unterminated raw string") {
                    self.errors.send(false).unwrap();
                } else {
                    self.emitter.emit(cmsp, msg, code, lvl);
                    self.errors.send(true).unwrap();
                    // Send any "help" messages that may follow
                    self.filter = false;
                }
            }
            _ => ()
        }
    }

    fn custom_emit(&mut self, _cm: &CodeMap, _sp: RenderSpan,
            _msg: &str, _lvl: Level) {
        panic!("ErrorEmitter does not implement custom_emit");
    }
}
