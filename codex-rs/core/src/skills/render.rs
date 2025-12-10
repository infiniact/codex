use crate::skills::model::SkillMetadata;

pub fn render_skills_section(skills: &[SkillMetadata]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }

    let mut lines: Vec<String> = Vec::new();
    lines.push("## Skills".to_string());
    lines.push("Skills are knowledge bases discovered from ~/.iaterm/{uuid}/skills. Each entry shows name, description, and file path. Skills provide reference implementations and templates - you should learn from them and generate appropriate commands for the current terminal environment.".to_string());

    for skill in skills {
        let path_str = skill.path.to_string_lossy().replace('\\', "/");
        lines.push(format!(
            "- {}: {} (file: {})",
            skill.name, skill.description, path_str
        ));
    }

    lines.push(
        r###"- Discovery: Available skills are listed above with name + description + file path. These are knowledge bases, not direct executables.
- Trigger rules: If the user names a skill (with `$SkillName` or plain text) OR the task clearly matches a skill's description, you must use that skill. Multiple mentions mean use them all.
- Missing/blocked: If a named skill isn't in the list or the path can't be read, say so briefly and continue with the best fallback.

- How to use a skill (IMPORTANT - Remote Execution Model):
  1) Skills are LOCAL knowledge bases. The terminal you're working with may be REMOTE (SSH, etc.).
  2) Use the skill's `name` and `description` (shown above) to understand its purpose.
  3) ONLY read additional files when truly necessary:
     - Read `scripts/` ONLY if you need implementation details for a specific step.
     - Read `references/` ONLY if you need domain knowledge not in the description.
     - Read `SKILL.md` body ONLY if the description is insufficient to understand the workflow.
  4) `assets/` contain templates - adapt them for the target environment rather than copying directly.
  5) NEVER assume local skill scripts can run on the remote terminal. Always generate fresh commands based on:
     - The remote OS/shell environment
     - Available tools on the remote system
     - The skill's reference implementation as a guide

- Context Hygiene (CRITICAL):
  1) DO NOT read SKILL.md unless the description above is insufficient.
  2) DO NOT read all scripts at once. Read only the specific script needed for the current step.
  3) The `description` field should contain enough info for most use cases - use it first.
  4) Minimize file reads to keep context small and efficient.

- Execution Feedback and Learning:
  1) After executing commands, observe the results carefully.
  2) If a command fails or produces unexpected results, note what went wrong.
  3) When you discover better approaches, environment-specific tweaks, or common errors, you SHOULD update the skill's knowledge base:
     - Add notes to `references/` documenting what works in different environments
     - Update `scripts/` with improved implementations or alternatives
     - Add `assets/` templates for common patterns
     - Update `SKILL.md` with lessons learned
  4) Format for feedback notes (create `references/execution-notes.md` if needed):
     ```
     ## Execution Log
     ### [Date] [Environment: e.g., Ubuntu 22.04, macOS, Alpine]
     - Command tried: `...`
     - Result: success/failure
     - Issue: (if failed)
     - Solution: (what worked)
     - Recommendation: (for future use)
     ```

- Description as trigger: The YAML `description` in `SKILL.md` is the primary trigger signal.
- Coordination: If multiple skills apply, choose the minimal set and state the order.
- Safety: If a skill can't be applied cleanly, state the issue and pick the best fallback."###
            .to_string(),
    );

    Some(lines.join("\n"))
}
