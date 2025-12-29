# Planning Scenario

## When to Use Plans
- Non-trivial multi-step tasks
- Logical phases/dependencies
- Ambiguous work needing outline
- Multiple items in single prompt
- User requests "TODOs"

## Plan Quality
High-quality: specific, actionable steps (5-7 words each)

Good examples:
1. Add CLI entry with file args
2. Parse Markdown via CommonMark
3. Apply semantic HTML template
4. Handle code blocks, images, links
5. Add error handling for invalid files

Bad examples (too vague):
1. Create CLI tool
2. Add parser
3. Make it work

## Plan Tool Usage
- `update_plan` with steps and status (`pending`/`in_progress`/`completed`)
- One `in_progress` step at a time
- Mark complete before moving on
- Don't repeat plan contents after update
