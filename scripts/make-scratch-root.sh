#!/bin/bash
# Builds a throwaway projj-style root for exercising the review server.
#
#   make-scratch-root.sh DIR [MANY]
#
# MANY adds that many more changed files, of very different lengths, for
# trying out what only shows with a long list.
set -euo pipefail
S="$1"
export MANY="${2:-0}"
mkdir -p "$S" && cd "$S"
git init -q -b main
R="$S/track/example.com/acme/widgets.git"
mkdir -p "$R/.repo"
(cd "$R/.repo" && jj --quiet git init --colocate)
(cd "$R/.repo" && jj --quiet workspace add --name sdk-0 ../sdk-0)
cd "$R/sdk-0"
printf 'target/\n' > .gitignore
mkdir -p src docs
python3 - <<'PY'
import os
open('src/lib.rs','w').write('\n'.join([f'pub fn step_{i}() -> u32 {{\n    {i}\n}}\n' for i in range(1,25)]))
open('docs/guide.md','w').write('# Widgets\n\n' + '\n'.join(f'Paragraph {i} explains how widget number {i} is assembled and tested.' for i in range(1,30)) + '\n')
open('notes.txt','w').write('alpha\nbeta\ngamma\n')
for n in range(int(os.environ['MANY'])):
    path = f'bulk/part_{n // 12:02}/file_{n:03}.txt'
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, 'w').write(''.join(f'line {i} of file {n}\n' for i in range(40 + (n * 37) % 260)))
PY
jj --quiet describe -m "Add widget steps and guide"
jj --quiet new
cd "$S"
printf '.jj/\ntrack/*/*/*/.repo/\n' > .gitignore
git add -A
git -c user.name=t -c user.email=t@example.com commit -q -m base
cd "$R/sdk-0"
python3 - <<'PY'
import os
s=open('src/lib.rs').read()
s=s.replace('pub fn step_2() -> u32 {\n    2\n}','pub fn step_2() -> u64 {\n    // widened for large batches\n    2_000\n}').replace('pub fn step_20() -> u32 {\n    20\n}','pub fn step_20() -> u32 {\n    20 + step_1()\n}')
open('src/lib.rs','w').write(s)
g=open('docs/guide.md').read().replace('Paragraph 5 explains how widget number 5 is assembled and tested.','Paragraph 5 explains how widget number 5 is assembled, inspected, and tested.')
open('docs/guide.md','w').write(g)
open('src/new_module.rs','w').write('//! Brand new module.\n\npub fn fresh() -> &\'static str {\n    "fresh"\n}\n')
os.remove('notes.txt')
for n in range(int(os.environ['MANY'])):
    path = f'bulk/part_{n // 12:02}/file_{n:03}.txt'
    lines = open(path).read().split('\n')
    # A few hunks in some files, the whole file rewritten in others.
    step = 3 if n % 5 == 0 else 23
    for i in range(n % step, len(lines) - 1, step):
        lines[i] += ' (changed)'
    open(path, 'w').write('\n'.join(lines))
    if n % 9 == 0:
        open(path.replace('file_', 'added_'), 'w').write(''.join(f'new line {i}\n' for i in range(5 + n)))
PY
jj --quiet describe -m "Widen step 2 and add new module"
jj --quiet new
printf 'pub fn later() {}\n' >> src/new_module.rs
jj --quiet describe -m "Add later()"
cd "$S"
printf 'scratch root\n' > README.md
