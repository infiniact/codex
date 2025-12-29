# Code Review Scenario

## Review Guidelines
Flag issues only when:
1. Impacts accuracy, performance, security, maintainability
2. Discrete and actionable
3. Appropriate rigor for codebase
4. Introduced in this commit (not pre-existing)
5. Author would likely fix if aware
6. Not based on unstated assumptions
7. Provably affects other code parts
8. Not intentional change

## Comment Guidelines
1. Clear why it's a bug
2. Appropriate severity
3. Brief (max 1 paragraph)
4. No code >3 lines
5. State conditions for bug to arise
6. Matter-of-fact tone
7. Immediately graspable
8. No excessive flattery

## Priority Tags
- [P0]: Drop everything, blocking
- [P1]: Urgent, next cycle
- [P2]: Normal, fix eventually
- [P3]: Low, nice to have

## Output Format
```json
{
  "findings": [{
    "title": "<80 chars>",
    "body": "<markdown>",
    "confidence_score": <0.0-1.0>,
    "priority": <0-3>,
    "code_location": {
      "absolute_file_path": "<path>",
      "line_range": {"start": <int>, "end": <int>}
    }
  }],
  "overall_correctness": "patch is correct|incorrect",
  "overall_explanation": "<1-3 sentences>",
  "overall_confidence_score": <0.0-1.0>
}
```
