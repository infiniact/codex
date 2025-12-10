pub mod amend;
pub mod decision;
pub mod error;
pub mod execpolicycheck;
pub mod policy_parser;
pub mod parser;
pub mod policy;
pub mod rule;
pub mod program;
pub mod exec_call;
pub mod arg_matcher;
pub mod arg_type;
pub mod valid_exec;
pub mod sed_command;
pub mod opt;
pub mod arg_resolver;

pub use amend::AmendError;
pub use amend::blocking_append_allow_prefix_rule;
pub use decision::Decision;
pub use error::Error;
pub use error::Result;
pub use execpolicycheck::ExecPolicyCheckCommand;
pub use policy_parser::PolicyParser;
pub use policy::Evaluation;
pub use policy::Policy;
pub use rule::Rule;
pub use rule::RuleMatch;
pub use rule::RuleRef;
pub use program::NegativeExamplePassedCheck;
pub use program::PositiveExampleFailedCheck;
pub use program::MatchedExec;
pub use exec_call::ExecCall;
pub use arg_matcher::ArgMatcher;
pub use arg_type::ArgType;
pub use valid_exec::MatchedArg;
pub use valid_exec::ValidExec;
pub use valid_exec::MatchedOpt;
pub use valid_exec::MatchedFlag;
pub use arg_resolver::PositionalArg;
pub use sed_command::parse_sed_command;

pub fn get_default_policy() -> anyhow::Result<Policy> {
    let source = "default.policy";
    let contents = include_str!("default.policy");
    let parser = policy_parser::PolicyParser::new(source, contents);
    let policy = parser.parse().map_err(|e| anyhow::Error::msg(e.to_string()))?;
    Ok(policy)
}
