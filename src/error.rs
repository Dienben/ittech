//! Error management.
//!
//! This module reimplements/modifies a lot of the default `nom` error behaviour to make it
//! actually suitable for debugging a binary parser such as this. It may be slower than the nom
//! version but what use is a parser that's fast but gives useless output..

use nom::error::{ErrorKind, ParseError};
use nom::{Err, IResult};
use nom::{Offset, Parser};
use std::borrow::Cow;
use std::fmt::{Debug, Write};
use std::iter;


/// This error type accumulates errors and their position when backtracking
/// through a parse tree. With some post processing (cf `examples/json.rs`),
/// it can be used to display user friendly error messages
#[derive(Clone, Debug, PartialEq)]
pub struct VerboseError<I> {
    /// List of errors accumulated by `VerboseError`, containing the affected
    /// part of input data, and some context
    pub errors: Vec<(I, VerboseErrorKind)>,
}

/// Error context for `VerboseError`
#[derive(Clone, Debug, PartialEq)]
pub enum VerboseErrorKind {
    /// String added by the `context` function
    Context(Cow<'static, str>),

    /// Error kind given by various nom parsers
    Nom(ErrorKind),
}

impl<I> ParseError<I> for VerboseError<I> {
    fn from_error_kind(input: I, kind: ErrorKind) -> Self {
        VerboseError {
            errors: vec![(input, VerboseErrorKind::Nom(kind))],
        }
    }

    fn append(input: I, kind: ErrorKind, mut other: Self) -> Self {
        other.errors.push((input, VerboseErrorKind::Nom(kind)));
        other
    }

    fn from_char(_input: I, _c: char) -> Self {
        unimplemented!("chars don't really make sense for a binary parser")
    }
}

/// This trait is required by the `context` combinator to add a string to an existing error
pub trait ContextError<I>: Sized {
    /// Creates a new error from an input position, a string and an existing error. This is used
    /// mainly in the [context()] combinator, to add user friendly information to errors when
    /// backtracking through a parse tree
    fn add_context(_input: I, _ctx: Cow<'static, str>, other: Self) -> Self {
        other
    }

    fn new(_input: I, _ctx: Cow<'static, str>) -> Self;
}

impl<I> ContextError<I> for VerboseError<I> {
    fn add_context(input: I, ctx: Cow<'static, str>, mut other: Self) -> Self {
        other.errors.push((input, VerboseErrorKind::Context(ctx)));
        other
    }

    fn new(input: I, ctx: Cow<'static, str>) -> Self {
        VerboseError {
            errors: vec![(input, VerboseErrorKind::Context(ctx))],
        }
    }
}

/// Create a new error from an input position, a static string and an existing error.
/// This is used mainly in the [context()] combinator, to add user friendly information
/// to errors when backtracking through a parse tree
pub fn context<I: Clone, E: ContextError<I>, F, O>(
    context: impl Fn() -> Cow<'static, str>,
    mut f: F,
) -> impl FnMut(I) -> IResult<I, O, E>
where
    F: Parser<I, O, E>,
{
    move |i: I| match f.parse(i.clone()) {
        Ok(o) => Ok(o),
        Err(Err::Incomplete(i)) => Err(Err::Incomplete(i.clone())),
        Err(Err::Error(e)) => Err(Err::Error(E::add_context(i.clone(), context(), e))),
        Err(Err::Failure(e)) => Err(Err::Failure(E::add_context(i.clone(), context(), e))),
    }
}

#[macro_export]
macro_rules! context {
    ( $parser: expr, $msg: literal $(,)? ) => {
        $crate::error::context(move || ::std::borrow::Cow::Borrowed($msg), $parser)
    };
    ( $parser: expr, $fmt: literal $(, $args: expr )+ $(,)? ) => {
        $crate::error::context(move || ::std::borrow::Cow::Owned(::std::format!($fmt, $($args),+)), $parser)
    };
    ( $parser: expr, $payload: expr $(,)? ) => {
        $crate::error::context(move || ::std::borrow::Cow::Owned($payload.to_string()), $parser)
    };
}

#[macro_export]
macro_rules! error {
    ( $input: expr, $msg: literal $(,)? ) => {
        E::new($input, ::std::borrow::Cow::Borrowed($msg))
    };
    ( $input: expr, $fmt: literal $(, $args: expr )+ $(,)? ) => {
        E::new($input, ::std::borrow::Cow::Owned(::std::format!($fmt, $($args),+)))
    };
    ( $input: expr, $payload: expr $(,)? ) => {
        E::new($input, ::std::borrow::Cow::Owned($payload.to_string()))
    };
}


#[macro_export]
macro_rules! bail {
    ($($tt:tt)*) => {
        return ::std::result::Result::Err(::nom::Err::Error($crate::error!($($tt)*)))
    };
}

/// Transforms a `VerboseError` into a trace with input position information.
///
/// This function is modified from the original [`nom::error::convert_error`] to be used with
/// binary input to a context. The trace is instead of lines shown on an `xxd`-style hexdump.
pub fn convert_error(
    input: &[u8],
    e: VerboseError<&[u8]>,
) -> String {
    // We're using `write!` on a `String` buffer, which is infallible so `unwrap`s here are fine.
    let mut result = String::new();

    for (i, (substring, kind)) in e.errors.iter().enumerate() {
        let offset = input.offset(substring);

        if input.is_empty() {
            use VerboseErrorKind::*;
            match kind {
                Context(s) => write!(&mut result, "{}: in {}, got empty input\n\n", i, s).unwrap(),
                Nom(e) => write!(&mut result, "{}: in {:?}, got empty input\n\n", i, e).unwrap(),
            }
        } else {
            // Find the line that includes the subslice.
            //
            // Our "line" is a 16-byte string, therefore the beginning of our line is just the
            // offset rounded down to the nearest multiple of 16.
            let line_begin = offset - (offset % 16);

            // Format the line into a hexdump, if there are not 16 bytes left till the end of the
            // input fill the rest with whitespace to match the alignment.
            //
            // A line should look like this:
            // 00000000: 0000 0000 0000 0000 0000 0000 0000 0000  ................
            // ^offset   ^16 bytes in {:02x} grouped by 2         ^ char if ascii printable or ' ',
            //  {:08x}                                              otherwise a '.'
            let line = {
                let mut buf = String::new();

                // offset
                write!(&mut buf, "{:08x}:", line_begin).unwrap();

                // hexdump
                let line = input[line_begin..]
                    .iter()
                    .map(|&byte| Some(byte))
                    .chain(iter::repeat(None))
                    .take(16)
                    .enumerate();

                for (i, byte) in line.into_iter() {
                    if i % 2 == 0 {
                        buf.push(' ');
                    }
                    if let Some(byte) = byte {
                        write!(&mut buf, "{:02x}", byte).unwrap();
                    } else {
                        buf.push_str("  ");
                    }
                }

                buf.push_str("  ");

                // ascii representation
                for &byte in input[line_begin..].iter().take(16) {
                    if byte.is_ascii_graphic() || byte == b' ' {
                        buf.push(byte as char);
                    } else {
                        buf.push('.');
                    }
                }

                buf
            };

            const CARET: &str = "^---";

            // The caret is positioned beneath the hex representation of the byte.
            let caret_position = {
                let line_offset = offset % 16;
                10 + CARET.len() + (line_offset / 2) * 5 + (line_offset % 2) * 2
            };

            match kind {
                VerboseErrorKind::Context(s) => write!(
                    &mut result,
                    "{i}: at offset {offset:#x}, {context}:\n\
                    {line}\n\
                    {caret:>column$}\n\n",
                    i = i,
                    offset = offset,
                    context = s,
                    line = line,
                    caret = CARET,
                    column = caret_position,
                ).unwrap(),
                VerboseErrorKind::Nom(_) => {},
                // write!(
                //     &mut result,
                //     "{i}: at offset {offset:#x}, in {nom_err:?}:\n\
                //     {line}\n\
                //     {caret:>column$}\n\n",
                //     i = i,
                //     offset = offset,
                //     nom_err = e,
                //     line = line,
                //     caret = CARET,
                //     column = caret_position,
                // ),
            }
        }
    }

    result
}
