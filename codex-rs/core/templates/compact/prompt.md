You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.

## Required Sections

### 1. Task Overview
- Original user request (verbatim if possible)
- Overall goal and success criteria

### 2. Current Progress
- Key decisions made
- **If creating/editing files**: Include file paths, current sizes, and completion status
- **If there is an active task plan**: Include its status with each step marked as pending/in_progress/completed

### 3. File Operations Status (CRITICAL)
If working on file creation or modification, MUST include:
- File path: `/path/to/file`
- File exists: yes/no
- File size: X bytes (if exists)
- Content status: empty/partial/complete
- Last successful operation: e.g., "wrote header section", "added function X"

### 4. Environment Issues Encountered
- List any sandbox restrictions or permission errors
- Commands that failed and why
- Successful workarounds found

### 5. Next Steps
- Clear, specific actions to continue (not restart)
- Any files to verify before proceeding

## Format for Plans
- [ ] Step 1 (pending)
- [→] Step 2 (in_progress) - "Currently: writing function X"
- [✓] Step 3 (completed)

**IMPORTANT**: Do NOT suggest restarting or recreating work that was already done. Focus on CONTINUING from the current state.
