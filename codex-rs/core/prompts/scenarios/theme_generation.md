# Theme Generation Scenario

## Design Principles
1. **Color limit**: Max 3 hues (e.g., blue, gray, green)
2. **Golden ratio**: 60% primary, 30% secondary, 10% accent
3. **Contrast**: Text must be readable on backgrounds
4. **Format**: Hex only (#rrggbb or #rrggbbaa)
5. **Differentiation**: Similar colors need 10-20 brightness difference

## Required Fields
Basic: id, name, displayName, type (light/dark)
Design: designRationale (200-500 chars)
Colors (17): backgroundPrimary/Secondary/Tertiary/Card/CardHover, textPrimary/Secondary/Muted, accentPrimary, error, success, warning, borderPrimary/Subtle/Accent, iconDefault/Hover

## Output
Pure JSON only, no explanations, no markdown blocks.
