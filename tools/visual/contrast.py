"""Audit default opaque sRGB tokens; disabled text and decorative dividers excluded."""
from pathlib import Path
import json
import re

root = Path(__file__).resolve().parents[2]
source = (root / 'crates/pod-ui/src/tokens.rs').read_text(encoding='utf-8')

def luminance(value):
    channels = [((value >> shift) & 255) / 255 for shift in (16, 8, 0)]
    linear = [c / 12.92 if c <= .04045 else ((c + .055) / 1.055) ** 2.4 for c in channels]
    return sum(c * w for c, w in zip(linear, (.2126, .7152, .0722)))

def ratio(a, b):
    a, b = luminance(a), luminance(b)
    return (max(a, b) + .05) / (min(a, b) + .05)

results = []
for mode in ('Dark', 'Light'):
    body = re.search(r'Mode::' + mode + r' => Self \{(.*?)\n            \}', source, re.S)[1]
    tokens = {name: int(value, 16) for name, value in re.findall(r'(\w+): 0x([A-Fa-f0-9]+)', body)}
    surfaces = ['bg_canvas', 'bg_chrome', 'bg_surface', 'bg_raised', 'bg_hover', 'bg_selected']
    pairs = [(fg, bg, 4.5) for fg in ('text_primary', 'text_secondary', 'text_muted') for bg in surfaces]
    pairs += [('accent_fg', bg, 3) for bg in surfaces]
    pairs += [('border_control', bg, 3) for bg in surfaces[:4]]
    pairs += [('action_primary_fg', bg, 4.5) for bg in ('action_primary_bg', 'action_primary_hover', 'action_primary_pressed')]
    pairs += [(tone+'_fg', tone+'_bg', 4.5) for tone in ('success', 'warning', 'danger', 'info')]
    pairs += [('terminal_fg', 'terminal_bg', 4.5)]
    for fg, bg, minimum in pairs:
        measured = ratio(tokens[fg], tokens[bg])
        results.append(dict(theme=mode, foreground=fg, background=bg, ratio=round(measured, 3), minimum=minimum, passed=measured >= minimum))
target = root / 'crates/pod-app/qa/d3/contrast.json'
target.parent.mkdir(parents=True, exist_ok=True)
target.write_text(json.dumps(results, indent=2)+'\n', encoding='utf-8')
for mode in ('Dark', 'Light'):
    rows = [row for row in results if row['theme'] == mode]
    print(f"{mode}: {sum(row['passed'] for row in rows)}/{len(rows)} AA token pairs pass")
for row in results:
    if not row['passed']:
        print(row)
raise SystemExit(0 if all(row['passed'] for row in results) else 1)
