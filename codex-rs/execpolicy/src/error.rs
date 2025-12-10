use thiserror::Error;
use serde::Serialize;
use crate::arg_matcher::ArgMatcher;
use crate::arg_resolver::PositionalArg;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, Serialize, PartialEq, Eq)]
pub enum Error {
    #[error("invalid decision: {0}")]
    InvalidDecision(String),
    #[error("invalid pattern element: {0}")]
    InvalidPattern(String),
    #[error("invalid example: {0}")]
    InvalidExample(String),
    #[error(
        "expected every example to match at least one rule. rules: {rules:?}; unmatched examples: \
         {examples:?}"
    )]
    ExampleDidNotMatch {
        rules: Vec<String>,
        examples: Vec<String>,
    },
    #[error("expected example to not match rule `{rule}`: {example}")]
    ExampleDidMatch { rule: String, example: String },
    #[error("starlark error: {0}")]
    Starlark(String),
    #[error("literal value did not match: expected {expected}, actual {actual}")]
    LiteralValueDidNotMatch { expected: String, actual: String },
    #[error("empty file name")]
    EmptyFileName {},
    #[error("invalid positive integer: {value}")]
    InvalidPositiveInteger { value: String },
    #[error("internal invariant violation: {message}")]
    InternalInvariantViolation { message: String },
    #[error("not enough args for {program}")]
    NotEnoughArgs {
        program: String,
        args: Vec<PositionalArg>,
        arg_patterns: Vec<ArgMatcher>,
    },
    #[error("prefix overlaps suffix")]
    PrefixOverlapsSuffix {},
    #[error("vararg matcher did not match anything for {program}")]
    VarargMatcherDidNotMatchAnything { program: String, matcher: ArgMatcher },
    #[error("unexpected arguments for {program}")]
    UnexpectedArguments { program: String, args: Vec<PositionalArg> },
    #[error("multiple vararg patterns for {program}")]
    MultipleVarargPatterns { program: String, first: ArgMatcher, second: ArgMatcher },
    #[error("range start exceeds end: {start} > {end}")]
    RangeStartExceedsEnd { start: usize, end: usize },
    #[error("range end out of bounds: {end} > len {len}")]
    RangeEndOutOfBounds { end: usize, len: usize },
    #[error("unknown option {option} for {program}")]
    UnknownOption { program: String, option: String },
    #[error("option {option} missing value for {program}")]
    OptionMissingValue { program: String, option: String },
    #[error("option followed by option instead of value: {option}={value} for {program}")]
    OptionFollowedByOptionInsteadOfValue { program: String, option: String, value: String },
    #[error("double dash not supported yet for {program}")]
    DoubleDashNotSupportedYet { program: String },
    #[error("missing required options for {program}: {options:?}")]
    MissingRequiredOptions { program: String, options: Vec<String> },
    #[error("sed command not provably safe: {command}")]
    SedCommandNotProvablySafe { command: String },
}
