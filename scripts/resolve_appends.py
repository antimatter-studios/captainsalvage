"""Resolves conflicts where two branches both appended to the end of a file: keeps both sides,
and in locale files keeps the first translation of any string listed twice."""
import re, subprocess, sys
files = subprocess.run(['git', 'diff', '--name-only', '--diff-filter=U'], capture_output=True, text=True).stdout.split()
for path in files:
    text = open(path).read()
    text = re.sub(r'<<<<<<< [^\n]*\n(.*?)=======\n(.*?)>>>>>>> [^\n]*\n', lambda m: m.group(1) + m.group(2), text, flags=re.S)
    if '<<<<<<<' in text or '>>>>>>>' in text:
        sys.exit(f'{path}: a conflict this cannot resolve')
    if path.startswith('src/locales/'):
        seen, kept = set(), []
        for line in text.split('\n'):
            key = re.match(r'\s*("(?:[^"\\]|\\.)*"):', line)
            if key:
                if key.group(1) in seen:
                    continue
                seen.add(key.group(1))
            kept.append(line)
        text = '\n'.join(kept)
    open(path, 'w').write(text)
    print('resolved', path)
