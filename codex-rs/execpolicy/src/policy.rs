use crate::decision::Decision;
use crate::error::Error;
use crate::error::Result;
use crate::exec_call::ExecCall;
use crate::program::NegativeExamplePassedCheck;
use crate::program::ProgramSpec;
use crate::program::MatchedExec;
use crate::rule::PatternToken;
use crate::rule::PrefixPattern;
use crate::rule::PrefixRule;
use crate::rule::RuleMatch;
use crate::rule::RuleRef;
use crate::policy_parser::ForbiddenProgramRegex;
use multimap::MultiMap;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

type HeuristicsFallback<'a> = Option<&'a dyn Fn(&[String]) -> Decision>;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Policy {
    rules_by_program: MultiMap<String, RuleRef>,
    programs: MultiMap<String, ProgramSpec>,
    forbidden_program_regexes: Vec<ForbiddenProgramRegex>,
    forbidden_substrings: Vec<String>,
}

impl Policy {
    pub fn from_rules(rules_by_program: MultiMap<String, RuleRef>) -> Self {
        Self {
            rules_by_program,
            programs: MultiMap::new(),
            forbidden_program_regexes: Vec::new(),
            forbidden_substrings: Vec::new(),
        }
    }

    pub fn new(
        programs: MultiMap<String, ProgramSpec>,
        forbidden_program_regexes: Vec<ForbiddenProgramRegex>,
        forbidden_substrings: Vec<String>,
    ) -> Self {
        Self {
            rules_by_program: MultiMap::new(),
            programs,
            forbidden_program_regexes,
            forbidden_substrings,
        }
    }

    pub fn empty() -> Self {
        Self::new(MultiMap::new(), Vec::new(), Vec::new())
    }

    pub fn rules(&self) -> &MultiMap<String, RuleRef> {
        &self.rules_by_program
    }

    pub fn add_prefix_rule(&mut self, prefix: &[String], decision: Decision) -> Result<()> {
        let (first_token, rest) = prefix
            .split_first()
            .ok_or_else(|| Error::InvalidPattern("prefix cannot be empty".to_string()))?;

        let rule: RuleRef = Arc::new(PrefixRule {
            pattern: PrefixPattern {
                first: Arc::from(first_token.as_str()),
                rest: rest
                    .iter()
                    .map(|token| PatternToken::Single(token.clone()))
                    .collect::<Vec<_>>()
                    .into(),
            },
            decision,
        });

        self.rules_by_program.insert(first_token.clone(), rule);
        Ok(())
    }

    pub fn check<F>(&self, cmd: &[String], heuristics_fallback: &F) -> Evaluation
    where
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules = self.matches_for_command(cmd, Some(heuristics_fallback));
        Evaluation::from_matches(matched_rules)
    }

    pub fn check_multiple<Commands, F>(
        &self,
        commands: Commands,
        heuristics_fallback: &F,
    ) -> Evaluation
    where
        Commands: IntoIterator,
        Commands::Item: AsRef<[String]>,
        F: Fn(&[String]) -> Decision,
    {
        let matched_rules: Vec<RuleMatch> = commands
            .into_iter()
            .flat_map(|command| {
                self.matches_for_command(command.as_ref(), Some(heuristics_fallback))
            })
            .collect();

        Evaluation::from_matches(matched_rules)
    }

    pub fn matches_for_command(
        &self,
        cmd: &[String],
        heuristics_fallback: HeuristicsFallback<'_>,
    ) -> Vec<RuleMatch> {
        let mut matched_rules: Vec<RuleMatch> = match cmd.first() {
            Some(first) => self
                .rules_by_program
                .get_vec(first)
                .map(|rules| rules.iter().filter_map(|rule| rule.matches(cmd)).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        };

        if let (true, Some(heuristics_fallback)) = (matched_rules.is_empty(), heuristics_fallback) {
            matched_rules.push(RuleMatch::HeuristicsRuleMatch {
                command: cmd.to_vec(),
                decision: heuristics_fallback(cmd),
            });
        }

        matched_rules
    }

    pub fn check_exec(&self, exec_call: &ExecCall) -> Result<MatchedExec> {
        let binding: Vec<ProgramSpec> = Vec::new();
        let specs = self.programs.get_vec(&exec_call.program).unwrap_or(&binding);
        // Try specs in order; return first match or forbidden
        for spec in specs {
            match spec.check(exec_call) {
                Ok(matched) => return Ok(matched),
                Err(_) => continue,
            }
        }
        Err(Error::InvalidExample(format!(
            "no matching program spec for {}",
            exec_call.program
        )))
    }

    pub fn check_each_bad_list_individually(&self) -> Vec<NegativeExamplePassedCheck> {
        let mut out = Vec::new();
        for (_name, specs) in self.programs.iter_all() {
            for spec in specs {
                out.extend(spec.verify_should_not_match_list());
            }
        }
        out
    }

    pub fn check_each_good_list_individually(&self) -> Vec<crate::program::PositiveExampleFailedCheck> {
        let mut out = Vec::new();
        for (_name, specs) in self.programs.iter_all() {
            for spec in specs {
                out.extend(spec.verify_should_match_list());
            }
        }
        out
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evaluation {
    pub decision: Decision,
    #[serde(rename = "matchedRules")]
    pub matched_rules: Vec<RuleMatch>,
}

impl Evaluation {
    pub fn is_match(&self) -> bool {
        self.matched_rules
            .iter()
            .any(|rule_match| !matches!(rule_match, RuleMatch::HeuristicsRuleMatch { .. }))
    }

    fn from_matches(matched_rules: Vec<RuleMatch>) -> Self {
        let decision = matched_rules
            .iter()
            .map(RuleMatch::decision)
            .max()
            .unwrap_or(Decision::Allow);

        Self {
            decision,
            matched_rules,
        }
    }
}
