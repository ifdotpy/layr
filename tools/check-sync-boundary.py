#!/usr/bin/env python3
"""Freeze legacy callers/definitions; engines and new transport helpers stay storage-free."""
import collections, json, os, re, sys
from pathlib import Path
ROOT = Path(__file__).resolve().parent.parent
BASE = ROOT / 'tools/sync-boundary-baseline.json'
APP = re.compile(r'\b(?:Repo|Ctx|Db|repo|ctx|sync_local|export_jsonl|lock_maintenance)\b|\.\s*db\b|\bdb\s*::|\bstore\s*::\s*Project\b|\bcrate\s*::\s*(?:repo|db|store|gitbridge|objstore|free|secrets)\b')
ENGINES = {'fetch_with_local', 'restore_with_local', 'record_left', 'push_with_local', 'prepare_push'}
LITERALS = re.compile(r'//[^\n]*|/\*|r(#*)"[\s\S]*?"\1|"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'', re.S)
BLOCK = re.compile(r'/\*|\*/')
def closing(text, start, left='{', right='}'):
    depth = 0
    for end in range(start, len(text)):
        depth += (text[end] == left) - (text[end] == right)
        if not depth: return end + 1
    raise ValueError('unbalanced Rust source')
def gated(condition):
    """Whether `test` is a conjunct of the cfg condition: only test builds have the item."""
    condition = condition.strip()
    if not (condition.startswith('all(') and condition.endswith(')')): return condition == 'test'
    depth, parts = 0, ['']
    for c in condition[4:-1]:
        depth += (c == '(') - (c == ')')
        if c == ',' and not depth: parts.append('')
        else: parts[-1] += c
    return any(gated(part) for part in parts)
def tested(attributes):
    return any(gated(condition) for condition in re.findall(r'#\[\s*cfg\(([^\]]*)\)\s*\]', attributes))
def mask(text):
    """`text` with comments (block comments nest), strings and chars blanked, at the same offsets."""
    out, at = [], 0
    while m := LITERALS.search(text, at):
        end, depth = m.end(), m[0] == '/*'
        while depth and (n := BLOCK.search(text, end)):
            depth, end = depth + (n[0] == '/*') - (n[0] == '*/'), n.end()
        end = len(text) if depth else end
        out += [text[at:m.start()], ' ' * (end - m.start())]
        at = end
    return ''.join(out) + text[at:]
def source(path):
    text = mask(path.read_text())
    for m in reversed([m for m in re.finditer(r'((?:#\[[^\]]*\]\s*)+)(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{', text) if tested(m[1])]):
        end = closing(text, text.index('{', m.start())); text = text[:m.start()] + ' ' * (end - m.start()) + text[end:]
    return text
DECLARATION = re.compile(r'((?:#\[[^\]\n]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?\bmod\s+(\w+)\s*;')
def test_files(root):
    """The files that only test declarations load: `#[cfg(test)] mod name;` (with or without `#[path]`) and, below
    such a file, any `mod child;`. A declaration outside an inline module takes its file; one inside an inline
    module, whose directory this scan does not follow, stays by module name and file stem."""
    found = []  # (declaring file, module name, `#[path]`, declared under cfg(test), inside an inline module)
    for path in (root / 'src').rglob('*.rs'):
        text = path.read_text()
        masked = mask(text)
        for m in DECLARATION.finditer(masked):
            external = re.search(r'\bpath\s*=\s*r?#*"([^"]*)"', text[m.start(1):m.end(1)])
            found.append((path, m[2], external and external[1], tested(m[1]),
                          masked.count('{', 0, m.start()) != masked.count('}', 0, m.start())))
    loaded = {Path(os.path.normpath(path.parent / external)) for path, _, external, _, _ in found if external}
    def files(path, name, external):
        base = path.parent if path.name in ('lib.rs', 'main.rs', 'mod.rs') or path.parent == root / 'src/bin' or path in loaded else path.parent / path.stem
        return [Path(os.path.normpath(file)) for file in ([path.parent / external] if external else [base / (name + '.rs'), base / name / 'mod.rs']) if file.exists()]
    def names(name, external):
        return {name} | ({Path(external).parent.name if Path(external).stem == 'mod' else Path(external).stem} if external else set())
    tests = {}  # file -> module names that load it
    while grown := {file: names(name, external) for path, name, external, test, _ in found if test or path in tests
                    for file in files(path, name, external) if file not in tests}:
        tests |= grown
    while True:  # A production declaration takes its file, and so every declaration in that file becomes production too.
        taken, taken_names = set(), set()
        for path, name, external, test, inline in found:
            if not test and path not in tests:
                taken_names |= names(name, external) if inline else set()
                taken |= set() if inline else set(files(path, name, external))
        production = [file for file, called in tests.items() if file in taken or called & taken_names]
        if not production: return {file.relative_to(root).as_posix() for file in tests}
        for file in production: del tests[file]
def body(text, start):
    """The `{` of the body of the function whose signature starts at `start`, or -1 for a declaration. A `;` or `{`
    inside parentheses, brackets (`[u8; 32]`) or generics (`-> T<{ 1 }>`) belongs to the signature. A generic `<`
    follows a name or `::` at once; `->`, `=>` and shifts are no delimiters."""
    stack = []
    for at in range(start, len(text)):
        c, before, after = text[at], text[at - 1], text[at + 1:at + 2]
        if c in '([' or (c == '{' and stack) or (c == '<' and (before.isalnum() or before in '_:') and after not in '<='):
            stack.append(c)
        elif stack and stack[-1] == {')': '(', ']': '[', '}': '{', '>': '<'}.get(c) and not (c == '>' and before in '-='):
            stack.pop()
        elif not stack and c in '{;': return at if c == '{' else -1
    return -1
def functions(text):
    for m in re.finditer(r'\bfn\s+(\w+)\s*[<(]', text):
        start = body(text, m.end() - 1)
        if start >= 0: yield m[1], text[m.start():closing(text, start)]
def transfer(text):
    for m in re.finditer(r'(?:\.|RemoteSync::|Store::)\s*(?:fetch|push|clone_into)\s*\(', text):
        depth = commas = 0
        for c in text[m.end():closing(text, m.end() - 1, '(', ')') - 1]:
            depth += (c in '([{') - (c in ')]}'); commas += c == ',' and depth == 0
        if commas >= 2: return True
    return False
def scan(root, frozen=None):
    data, errors = {'$callers': [], '$references': [], '$access': {}}, []
    tests = test_files(root)
    for path in sorted((root / 'src').rglob('*.rs')):
        name, text = path.relative_to(root).as_posix(), source(path)
        if name in tests: continue
        if name.startswith('src/store_remote/'):
            definitions = list(functions(text)); data[name] = dict(collections.Counter(n for n, _ in definitions))
            known = frozen.get(name, {}) if frozen is not None else data[name]
            if frozen is not None and any(n in known and count > known[n] for n, count in data[name].items()): errors.append(name + ': duplicate legacy function name')
            if frozen is not None and name not in frozen and APP.search(text): errors.append(name + ': new transport file captures local storage')
            # Each legacy function keeps exactly its frozen local-storage accesses (by token): a new one fails.
            access = collections.defaultdict(collections.Counter)
            for n, body in definitions: access[n].update(re.sub(r'\s+', '', m[0]) for m in APP.finditer(body))
            data['$access'][name] = {n: dict(sorted(c.items())) for n, c in sorted(access.items()) if c}
            legacy = frozen.get('$access', {}).get(name, {}) if frozen is not None else data['$access'][name]
            for n, used in access.items():
                allowed = {} if n in ENGINES or (path.name == 'gitpack.rs' and n in {'push', 'fetch'}) or n not in known else legacy.get(n, {})
                if any(count > allowed.get(token, 0) for token, count in used.items()): errors.append(name + ':' + n + ': local storage in transport helper')
        else:
            if transfer(text): data['$callers'].append(name)
            if re.search(r'\bstore_remote\b', text): data['$references'].append(name)
            if name.startswith('src/sync_local/') and re.search(r'\bBackend\b|\bbackend\s*::', text): errors.append(name + ': backend in local adapter')
    if frozen is not None:
        for key in ['$callers', '$references']:
            errors += [name + ': new direct transport caller/reference' for name in set(data[key]) - set(frozen[key])]
    contract = root / 'crates/layr-store/src/sync.rs'
    if contract.exists() and (APP.search(source(contract)) or re.search(r'\b(?:reqwest|rusqlite|layr_model|Backend)\b|(?:crate|super)::(?:http|s3)\b', source(contract))): errors.append('sync contract imports application/HTTP/DB types')
    return data, errors
def check(root=ROOT):
    return scan(root, json.loads(BASE.read_text()))[1]
if __name__ == '__main__':
    writing = '--write' in sys.argv
    data, errors = scan(ROOT, None if writing else json.loads(BASE.read_text()))
    if errors: sys.exit('\n'.join(errors))
    if writing: BASE.write_text('{\n' + ',\n'.join('  ' + json.dumps(k) + ': ' + json.dumps(v, sort_keys=True) for k, v in data.items()) + '\n}\n')
    print('Sync boundary OK' + ('; frozen sets written' if writing else ''))
