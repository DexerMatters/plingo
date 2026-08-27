pub mod persistent_seq;
pub use persistent_seq::{CountMeasure, PersistentSeq, SeqMeasure};
use core::fmt;
use std::{error::Error, fmt::Debug, sync::Arc};

use color_print::cwrite;
use fluent_uri::Uri;
use ropey::{Rope, RopeSlice};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

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

pub trait RangeExt: Sized {
    type Unit: Copy + Ord;

    fn covers(&self, other: &Self) -> bool;
    fn inside(&self, other: &Self) -> bool;
    fn overlaps(&self, other: &Self) -> bool;
    fn union(&self, other: &Self) -> Self;
    fn intersection(&self, other: &Self) -> Option<Self>;
    fn trim_to(&self, max: Self::Unit) -> Self;
}

impl<T: Copy + Ord> RangeExt for RangeOrPoint<T> {
    type Unit = T;

    fn covers(&self, other: &Self) -> bool {
        self.start() <= other.start() && self.end() >= other.end()
    }

    fn inside(&self, other: &Self) -> bool {
        self.start() >= other.start() && self.end() <= other.end()
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.start() < other.end() && self.end() > other.start()
    }

    fn union(&self, other: &Self) -> Self {
        RangeOrPoint::from_range(self.start().min(other.start()), self.end().max(other.end()))
    }

    fn intersection(&self, other: &Self) -> Option<Self> {
        let start = self.start().max(other.start());
        let end = self.end().min(other.end());
        if start < end {
            Some(RangeOrPoint::from_range(start, end))
        } else {
            None
        }
    }

    fn trim_to(&self, max: Self::Unit) -> Self {
        RangeOrPoint::from_range(self.start().min(max), self.end().min(max))
    }
}

/// An owned, `'static` slice of a [`Rope`]. Cheap to clone and keeps the
/// underlying rope alive via `Arc`.
#[derive(Clone)]
pub struct OwnedRopeSlice {
    rope: Arc<Rope>,
    start: usize,
    end: usize,
}

impl OwnedRopeSlice {
    pub fn new(rope: Arc<Rope>, start: usize, end: usize) -> Self {
        Self { rope, start, end }
    }

    /// Borrow the underlying [`RopeSlice`] with a closure — zero allocation.
    pub fn with_slice<R>(&self, f: impl FnOnce(RopeSlice<'_>) -> R) -> R {
        let start = self.rope.byte_to_char(self.start);
        let end = self.rope.byte_to_char(self.end);
        f(self.rope.slice(start..end))
    }

    pub fn to_string(&self) -> String {
        self.with_slice(|s| s.to_string())
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl fmt::Debug for OwnedRopeSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedRopeSlice")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish()
    }
}

impl fmt::Display for OwnedRopeSlice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.with_slice(|s| {
            for chunk in s.chunks() {
                f.write_str(chunk)?;
            }
            Ok(())
        })
    }
}

impl PartialEq for OwnedRopeSlice {
    fn eq(&self, other: &Self) -> bool {
        self.with_slice(|a| other.with_slice(|b| a == b))
    }
}

impl Eq for OwnedRopeSlice {}

/// A span in a source file, represented by a URI and a range of byte offsets.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Span {
    pub uri: Uri<String>,
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
        // Interning bounds leaked allocations to one per distinct URI string
        // rather than one per Span construction (plan §6 interim improvement).
        let uri: Uri<String> = uri.to_string().parse().map_err(SpanError::InvalidUri)?;
        Ok(Span {
            uri,
            range: RangeOrPoint::from_range(start, end),
        })
    }

    /// Creates a new `Span` with the given URI and byte offsets, using a
    /// pre-parsed URI.
    pub fn new_uri(uri: Uri<String>, start: usize, end: usize) -> Result<Self, SpanError> {
        if start > end {
            return Err(SpanError::IllegalSpan { start, end });
        }
        Ok(Span {
            uri,
            range: RangeOrPoint::from_range(start, end),
        })
    }

    /// Creates a `Span` that represents a single point (i.e., where start and
    /// end are the same).
    pub fn point(uri: String, offset: usize) -> Result<Self, SpanError> {
        Self::new(uri, offset, offset)
    }

    /// Creates a `Span` that represents a single point (i.e., where start and
    /// end are the same), using a pre-parsed URI.
    pub fn point_uri(uri: Uri<String>, offset: usize) -> Result<Self, SpanError> {
        Self::new_uri(uri, offset, offset)
    }

    /// Checks if this span covers another span (i.e., if it starts before or at
    /// the same position and ends after or at the same position).
    pub fn covers(&self, other: &Span) -> bool {
        self.uri == other.uri && self.range.covers(&other.range)
    }

    /// Checks if this span is inside another span
    /// (i.e., if it starts after or at the same position and ends before or at the same position).
    pub fn inside(&self, other: &Span) -> bool {
        self.uri == other.uri && self.range.inside(&other.range)
    }

    /// Checks if this span overlaps with another span
    /// (i.e., if they share any common byte positions).
    pub fn overlaps(&self, other: &Span) -> bool {
        self.uri == other.uri && self.range.overlaps(&other.range)
    }

    /// Computes the union of this span with another span, if they overlap or are adjacent.
    pub fn union(&self, other: &Span) -> Option<Span> {
        if self.uri != other.uri {
            return None;
        }
        Some(Span {
            uri: self.uri.clone(),
            range: self.range.union(&other.range),
        })
    }

    /// Computes the intersection of this span with another span, if they overlap.
    pub fn intersection(&self, other: &Span) -> Option<Span> {
        if self.uri != other.uri {
            return None;
        }
        self.range.intersection(&other.range).map(|range| Span {
            uri: self.uri.clone(),
            range,
        })
    }

    /// Trims the span to fit within the bounds of the given source text.
    pub fn trim(&self, source: &Rope) -> Span {
        Span {
            uri: self.uri.clone(),
            range: self.range.trim_to(source.len_bytes()),
        }
    }

    /// Converts this byte-addressed span to zero-based Rope line/character
    /// coordinates using the provided source text.
    pub fn to_line_col(&self, source: &Rope) -> RangeOrPoint<(usize, usize)> {
        let line_col = |byte: usize| {
            let character = source.byte_to_char(byte.min(source.len_bytes()));
            let line = source.char_to_line(character);
            (line, character - source.line_to_char(line))
        };
        RangeOrPoint::from_range(line_col(self.range.start()), line_col(self.range.end()))
    }

    pub fn map_range(&self, f: impl FnOnce(RangeOrPoint<usize>) -> RangeOrPoint<usize>) -> Self {
        Span {
            uri: self.uri.clone(),
            range: f(self.range),
        }
    }

    pub fn free_right(&self) -> Self {
        self.map_range(|range| match range {
            RangeOrPoint::Range(start, _) => RangeOrPoint::Range(start, usize::MAX),
            RangeOrPoint::Point(offset) => RangeOrPoint::Range(offset, usize::MAX),
        })
    }

    pub fn free_left(&self) -> Self {
        self.map_range(|range| match range {
            RangeOrPoint::Range(_, end) => RangeOrPoint::Range(0, end),
            RangeOrPoint::Point(offset) if offset != 0 => RangeOrPoint::Range(0, offset),
            x => x,
        })
    }

    pub fn extend_right(&self, amount: usize) -> Self {
        self.map_range(|range| match range {
            RangeOrPoint::Range(start, end) => {
                RangeOrPoint::Range(start, end.saturating_add(amount))
            }
            RangeOrPoint::Point(offset) => {
                RangeOrPoint::Range(offset, offset.saturating_add(amount))
            }
        })
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spanned<T> {
    pub span: Span,
    pub value: T,
}

/// A trait for types that can be pretty-printed with additional context
pub trait PrettyDisplay<Ctx = ()> {
    /// Returns a wrapper that enables pretty-printing of this value with the
    /// given context
    fn pretty<'a>(&'a self, context: &'a Ctx) -> PrettyWrapper<'a, Self, Ctx>
    where
        Self: Sized,
    {
        PrettyWrapper {
            value: self,
            context,
        }
    }

    /// Formats this value using the provided context, writing the result to the
    /// given formatter
    fn pretty_fmt(&self, f: &mut fmt::Formatter<'_>, context: &Ctx) -> fmt::Result;
}

impl<T: PrettyDisplay<Ctx>, Ctx> PrettyDisplay<Ctx> for &T {
    fn pretty_fmt(&self, f: &mut fmt::Formatter<'_>, context: &Ctx) -> fmt::Result {
        (*self).pretty_fmt(f, context)
    }
}

impl<T: PrettyDisplay<Ctx>, Ctx> PrettyDisplay<Ctx> for Vec<T> {
    fn pretty_fmt(&self, f: &mut fmt::Formatter<'_>, context: &Ctx) -> fmt::Result {
        self.iter()
            .map(|item| item.pretty(context))
            .fold(Ok(()), |res, pretty_item| {
                res.and_then(|_| cwrite!(f, "<dim>- </dim>{}\n", pretty_item))
            })
    }
}

/// A wrapper type that implements `Display` by delegating to the
/// `PrettyDisplay` implementation of the inner value, using the provided
/// context.
pub struct PrettyWrapper<'a, T, Ctx> {
    pub value: &'a T,
    pub context: &'a Ctx,
}

impl<T: PrettyDisplay<Ctx>, Ctx> fmt::Display for PrettyWrapper<'_, T, Ctx> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.value.pretty_fmt(f, self.context)
    }
}
