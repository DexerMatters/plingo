use core::fmt;
use std::{error::Error, fmt::Debug};

use fluent_uri::Uri;
use ropey::Rope;
use thiserror::Error;

/// A value with associated warnings
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Warned<T, W: Error> {
    pub warnings: Vec<W>,
    pub value: T,
}

impl<T, W: Error> Warned<T, W> {
    /// Creates a value with no warnings
    pub fn new(value: T) -> Self {
        Self {
            warnings: Vec::new(),
            value,
        }
    }

    /// Attaches a warning to the value
    pub fn warn(mut self, warning: W) -> Self {
        self.warnings.push(warning);
        self
    }

    /// Attaches multiple warnings to the value
    pub fn warn_many(mut self, warnings: Vec<W>) -> Self {
        self.warnings.extend(warnings);
        self
    }

    /// Transforms the value while keeping the warnings
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Warned<U, W> {
        Warned {
            warnings: self.warnings,
            value: f(self.value),
        }
    }

    /// Transforms the warnings while keeping the value
    pub fn map_warnings<U: Error>(mut self, f: impl FnMut(W) -> U) -> Warned<T, U> {
        let warnings = self.warnings.drain(..).map(f).collect();
        Warned {
            warnings,
            value: self.value,
        }
    }

    /// Checks if there are no warnings
    pub fn is_ok(&self) -> bool {
        self.warnings.is_empty()
    }
}

pub trait LiftAnyErrorFromVec<T, E> {
    fn lift(self) -> Result<Vec<T>, Vec<E>>;
}

impl<T: Debug, E: Debug> LiftAnyErrorFromVec<T, E> for Vec<Result<T, E>> {
    fn lift(self) -> Result<Vec<T>, Vec<E>> {
        let (oks, errs): (Vec<_>, Vec<_>) = self.into_iter().partition(Result::is_ok);
        if errs.is_empty() {
            Ok(oks.into_iter().map(Result::unwrap).collect())
        } else {
            Err(errs.into_iter().map(Result::unwrap_err).collect())
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq, Clone, Copy, Hash)]
pub enum RangeOrPoint<T: Copy + PartialEq> {
    Range(T, T),
    Point(T),
}

impl<T: Copy + PartialEq> RangeOrPoint<T> {
    pub fn from_range(start: T, end: T) -> Self {
        if start == end {
            RangeOrPoint::Point(start)
        } else {
            RangeOrPoint::Range(start, end)
        }
    }

    pub fn start(&self) -> T {
        match self {
            RangeOrPoint::Range(start, _) => *start,
            RangeOrPoint::Point(offset) => *offset,
        }
    }

    pub fn end(&self) -> T {
        match self {
            RangeOrPoint::Range(_, end) => *end,
            RangeOrPoint::Point(offset) => *offset,
        }
    }

    pub fn is_point(&self) -> bool {
        matches!(self, RangeOrPoint::Point(_))
    }
}

impl<T: Copy + PartialEq> fmt::Display for RangeOrPoint<T>
where
    T: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeOrPoint::Range(start, end) => write!(f, "{}-{}", start, end),
            RangeOrPoint::Point(offset) => write!(f, "{}", offset),
        }
    }
}

impl<T: Copy + PartialEq> From<(T, T)> for RangeOrPoint<T> {
    fn from((start, end): (T, T)) -> Self {
        RangeOrPoint::from_range(start, end)
    }
}

impl<T: Copy + PartialEq> From<RangeOrPoint<T>> for (T, T) {
    fn from(range_or_point: RangeOrPoint<T>) -> Self {
        match range_or_point {
            RangeOrPoint::Range(start, end) => (start, end),
            RangeOrPoint::Point(offset) => (offset, offset),
        }
    }
}

/// A span in a source file, represented by a URI and a range of byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub uri: Uri<&'static str>,
    pub range: RangeOrPoint<usize>,
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.range {
            RangeOrPoint::Range(start, end) => write!(f, "{}:{}-{}", self.uri, start, end),
            RangeOrPoint::Point(offset) => write!(f, "{}:{}", self.uri, offset),
        }
    }
}

impl Span {
    /// Creates a new `Span` with the given URI and byte offsets.
    pub fn new(uri: impl ToString, start: usize, end: usize) -> Result<Self, SpanError> {
        if start > end {
            return Err(SpanError::IllegalSpan { start, end });
        }
        let uri = Uri::parse(uri.to_string().leak() as &'static str)
            .map_err(|e| SpanError::InvalidUri(e))?;
        Ok(Span {
            uri,
            range: RangeOrPoint::Range(start, end),
        })
    }

    /// Creates a new `Span` with the given URI and byte offsets, using a pre-parsed URI.
    pub fn new_uri(uri: Uri<&'static str>, start: usize, end: usize) -> Result<Self, SpanError> {
        if start > end {
            return Err(SpanError::IllegalSpan { start, end });
        }
        Ok(Span {
            uri,
            range: RangeOrPoint::from_range(start, end),
        })
    }

    /// Creates a `Span` that represents a single point (i.e., where start and end are the same).
    pub fn point(uri: String, offset: usize) -> Result<Self, SpanError> {
        Self::new(uri, offset, offset)
    }

    /// Creates a `Span` that represents a single point (i.e., where start and end are the same), using a pre-parsed URI.
    pub fn point_uri(uri: Uri<&'static str>, offset: usize) -> Result<Self, SpanError> {
        Self::new_uri(uri, offset, offset)
    }

    /// Checks if this span covers another span
    /// (i.e., if it starts before or at the same position and ends after or at the same position).
    pub fn covers(&self, other: &Span) -> bool {
        self.uri == other.uri
            && self.range.start() <= other.range.start()
            && self.range.end() >= other.range.end()
    }

    /// Checks if this span is inside another span
    /// (i.e., if it starts after or at the same position and ends before or at the same position).
    pub fn inside(&self, other: &Span) -> bool {
        self.uri == other.uri
            && self.range.start() >= other.range.start()
            && self.range.end() <= other.range.end()
    }

    /// Checks if this span overlaps with another span
    /// (i.e., if they share any common byte positions).
    pub fn overlaps(&self, other: &Span) -> bool {
        self.uri == other.uri
            && self.range.start() < other.range.end()
            && self.range.end() > other.range.start()
    }

    /// Computes the union of this span with another span, if they overlap or are adjacent.
    pub fn union(&self, other: &Span) -> Option<Span> {
        if self.uri != other.uri {
            return None;
        }
        Some(Span {
            uri: self.uri,
            range: RangeOrPoint::from_range(
                self.range.start().min(other.range.start()),
                self.range.end().max(other.range.end()),
            ),
        })
    }

    /// Computes the intersection of this span with another span, if they overlap.
    pub fn intersection(&self, other: &Span) -> Option<Span> {
        if self.uri != other.uri {
            return None;
        }
        let start = self.range.start().max(other.range.start());
        let end = self.range.end().min(other.range.end());
        if start < end {
            Some(Span {
                uri: self.uri,
                range: RangeOrPoint::from_range(start, end),
            })
        } else {
            None
        }
    }

    /// Trims the span to fit within the bounds of the given source text.
    pub fn trim(&self, source: &Rope) -> Span {
        let start = self.range.start();
        let end = self.range.end();
        let source_len = source.len_bytes();
        Span {
            uri: self.uri,
            range: RangeOrPoint::from_range(start.min(source_len), end.min(source_len)),
        }
    }

    /// Converts the char offsets in this span to line and column numbers using the provided source text.
    pub fn to_line_col(&self, source: &Rope) -> RangeOrPoint<(usize, usize)> {
        let start_line = source.char_to_line(self.range.start());
        let start_col = self.range.start() - source.line_to_char(start_line);
        let end_line = source.char_to_line(self.range.end());
        let end_col = self.range.end() - source.line_to_char(end_line);
        RangeOrPoint::from_range((start_line, start_col), (end_line, end_col))
    }
}

/// An error type for span-related errors, including invalid URIs and illegal spans.
#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum SpanError {
    #[error("Invalid URI: {0}")]
    InvalidUri(fluent_uri::ParseError),
    #[error("Illegal span: start={start}, end={end}")]
    IllegalSpan { start: usize, end: usize },
}

/// A value with an associated span
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Spanned<T> {
    pub span: Span,
    pub value: T,
}
