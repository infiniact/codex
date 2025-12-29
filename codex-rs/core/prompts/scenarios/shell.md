# Shell Commands Scenario

## Guidelines
- Prefer `rg` over `grep` (faster)
- Read files in max 250-line chunks
- Output truncates at 10KB or 256 lines

## Common Patterns
```bash
# Search text
rg "pattern" path/

# Find files
rg --files path/ | rg "pattern"

# Read chunk
head -n 250 file.txt
sed -n '251,500p' file.txt
```
