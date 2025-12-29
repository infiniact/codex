# Sandbox and Approvals

## Filesystem Sandboxing
- **read-only**: Read only
- **workspace-write**: Read all, write workspace only
- **danger-full-access**: No sandbox

## Network Sandboxing
- **restricted** / **enabled**

## Approval Modes
- **untrusted**: Most commands need approval
- **on-failure**: Run in sandbox, failures escalated
- **on-request**: Sandbox default, can request escalation
- **never**: Non-interactive, never ask, must complete task

## When to Request Approval (on-request mode)
- Commands writing outside allowed dirs
- GUI apps (browsers, file openers)
- Network access when sandboxed
- Destructive actions (`rm`, `git reset`)
- Commands failing due to sandbox

Default assumption: workspace-write, network ON, approval on-failure
