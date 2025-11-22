## Custom Prompts

Custom prompts turn your repeatable instructions into reusable slash commands, so you can trigger them without retyping or copy/pasting. Each prompt is a Markdown file that Codex expands into the conversation the moment you run it.

### Where prompts live

- Location: store prompts in `$CODEX_HOME/prompts/` (defaults to `~/.codex/prompts/`). Set `CODEX_HOME` if you want to use a different folder.
- File type: Codex only loads `.md` files. Non-Markdown files are ignored. Both regular files and symlinks to Markdown files are supported.
- Naming: The filename (without `.md`) becomes the prompt name. A file called `review.md` registers the prompt `review`.
- Refresh: Prompts are loaded when a session starts. Restart Codex (or start a new session) after adding or editing files.
- Conflicts: Files whose names collide with built-in commands (like `init`) stay hidden in the slash popup, but you can still invoke them with `/prompts:<name>`.

### File format

- Body: The file contents are sent verbatim when you run the prompt (after placeholder expansion).
- Frontmatter (optional): Add YAML-style metadata at the top of the file to improve the slash popup.

  ```markdown
  ---
  description: Request a concise git diff review
  argument-hint: FILE=<path> [FOCUS=<section>]
  ---
  ```

  - `description` shows under the entry in the popup.
  - `argument-hint` (or `argument_hint`) lets you document expected inputs, though the current UI ignores this metadata.

### Placeholders and arguments

- Numeric placeholders: `$1`–`$9` insert the first nine positional arguments you type after the command. `$ARGUMENTS` inserts all positional arguments joined by a single space. Use `$$` to emit a literal dollar sign (Codex leaves `$$` untouched).
- Named placeholders: Tokens such as `$FILE` or `$TICKET_ID` expand from `KEY=value` pairs you supply. Keys are case-sensitive—use the same uppercase name in the command (for example, `FILE=...`).
- Quoted arguments: Double-quote any value that contains spaces, e.g. `TICKET_TITLE="Fix logging"`.
- Invocation syntax: Run prompts via `/prompts:<name> ...`. When the slash popup is open, typing either `prompts:` or the bare prompt name will surface `/prompts:<name>` suggestions.
- Error handling: If a prompt contains named placeholders, Codex requires them all. You will see a validation message if any are missing or malformed.

### Running a prompt

1. Start a new Codex session (ensures the prompt list is fresh).
2. In the composer, type `/` to open the slash popup.
3. Type `prompts:` (or start typing the prompt name) and select it with ↑/↓.
4. Provide any required arguments, press Enter, and Codex sends the expanded content.

### Examples

### Example 1: Basic named arguments

**File**: `~/.codex/prompts/ticket.md`

```markdown
---
description: Generate a commit message for a ticket
argument-hint: TICKET_ID=<id> TICKET_TITLE=<title>
---

Please write a concise commit message for ticket $TICKET_ID: $TICKET_TITLE
```

**Usage**:

```
/prompts:ticket TICKET_ID=JIRA-1234 TICKET_TITLE="Fix login bug"
```

**Expanded prompt sent to Codex**:

```
Please write a concise commit message for ticket JIRA-1234: Fix login bug
```

**Note**: Both `TICKET_ID` and `TICKET_TITLE` are required. If either is missing, Codex will show a validation error. Values with spaces must be double-quoted.

### Example 2: Mixed positional and named arguments

**File**: `~/.codex/prompts/review.md`

```markdown
---
description: Review code in a specific file with focus area
argument-hint: FILE=<path> [FOCUS=<section>]
---

Review the code in $FILE. Pay special attention to $FOCUS.
```

**Usage**:

```
/prompts:review FILE=src/auth.js FOCUS="error handling"
```

**Expanded prompt**:

```
Review the code in src/auth.js. Pay special attention to error handling.

```

## 合并 main 到 iaterm（2025-11-22）

- 目标：将 `main` 的最新提交合并到 `iaterm` 分支，并完成冲突处理。
- 环境：Mac（工作区 `/Users/xudatie/Documents/Code/codex`）。

### Sequential Thinking（步骤）
- 步骤1：检查分支与远端同步状态。
- 步骤2：更新本地 `main` 并切回 `iaterm`。
- 步骤3：合并 `main` 到 `iaterm`。
- 步骤4：对冲突文件采用主分支版本（除文档外），清理冲突标记。
- 步骤5：验证构建与最终结果记录。

### 执行与输出（节选）
- 命令：
  ```bash
  git fetch --all --prune && git checkout main && git pull --ff-only && git checkout iaterm && git merge --no-ff main
  ```
  结果：出现若干冲突（`Cargo.lock`、`core/*`、`tui/*`、`windows-sandbox-rs/*`、`docs/prompts.md`）。

- 解决策略：
  - 代码文件统一采用 `main` 侧版本（`--theirs`），以保持与主线一致。
  - `docs/prompts.md` 手工合并，保留主线文档并追加本次记录。

- 验证：
  ```bash
  cargo check
  ```
  构建检查完成。

### 结论
- 已将 `main` 合并进入 `iaterm` 并解决冲突；文档追加了本次操作记录。
