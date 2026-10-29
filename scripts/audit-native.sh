#!/usr/bin/env bash
#
# scripts/audit-native.sh
#
# Audits dependencies of `latch-host` across target platforms for native code:
#   (a) Build dependencies on cc, cmake, nasm-rs, bindgen, autotools, etc.
#   (b) C/C++/assembly source files (*.c, *.cc, *.cpp, *.cxx, *.h, *.hpp, *.S, *.s, *.asm).
#   (c) Packages declaring a `links` key (reporting pkg-config users as linking system libraries).
#
# Exits non-zero if any package under (a) or (b) is not listed in scripts/audit-native.allow.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ALLOW_FILE="$REPO_ROOT/scripts/audit-native.allow"

# Ensure cargo and python3 are available
if ! command -v cargo >/dev/null 2>&1; then
    if [ -f "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    else
        echo "Error: 'cargo' not found in PATH" >&2
        exit 1
    fi
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "Error: 'python3' not found in PATH" >&2
    exit 1
fi

TARGETS=(
    "aarch64-apple-darwin"
    "x86_64-pc-windows-msvc"
    "x86_64-unknown-linux-gnu"
    "aarch64-unknown-linux-gnu"
)

if [ "$#" -gt 0 ]; then
    TARGETS=("$@")
fi

python3 - "$REPO_ROOT" "$ALLOW_FILE" "${TARGETS[@]}" << 'EOF'
import sys
import os
import json
import subprocess

repo_root = sys.argv[1]
allow_file = sys.argv[2]
targets = sys.argv[3:]

# 1. Parse allow list
allowed = {}
if os.path.exists(allow_file):
    with open(allow_file, 'r', encoding='utf-8') as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith('#'):
                continue
            parts = line.split(None, 1)
            pkg_name = parts[0]
            reason = parts[1] if len(parts) > 1 else ""
            allowed[pkg_name] = reason

BUILD_TOOL_NAMES = {'cc', 'cmake', 'nasm-rs', 'autotools', 'bindgen'}
NATIVE_EXTENSIONS = ('.c', '.cc', '.cpp', '.cxx', '.h', '.hpp', '.S', '.s', '.asm')

has_unallowed_violations = False

print("=" * 80)
print("LATCH NATIVE CODE AUDIT")
print(f"Allow file: {os.path.relpath(allow_file, repo_root)} ({len(allowed)} allowed entries)")
print("=" * 80)

for target in targets:
    print(f"\nTarget Triple: {target}")
    print("-" * 80)

    cmd = [
        'cargo', 'metadata',
        '--format-version', '1',
        '--locked',
        '--manifest-path', os.path.join(repo_root, 'Cargo.toml'),
        '--filter-platform', target
    ]
    try:
        proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, cwd=repo_root)
        meta = json.loads(proc.stdout)
    except subprocess.CalledProcessError as e:
        print(f"Error running cargo metadata for target {target}:\n{e.stderr.decode('utf-8')}", file=sys.stderr)
        sys.exit(1)

    # Locate latch-host root package
    root_pkg = next((p for p in meta['packages'] if p['name'] == 'latch-host'), None)
    if not root_pkg:
        print("Error: Could not find package 'latch-host' in workspace metadata", file=sys.stderr)
        sys.exit(1)

    root_id = root_pkg['id']
    nodes = {n['id']: n for n in meta['resolve']['nodes']}
    pkgs = {p['id']: p for p in meta['packages']}

    # Walk transitive resolved dependencies (normal + build deps, excluding dev-only)
    visited = set()
    queue = [root_id]
    while queue:
        curr = queue.pop(0)
        if curr in visited:
            continue
        visited.add(curr)
        node = nodes.get(curr)
        if not node:
            continue
        for dep in node.get('deps', []):
            dep_id = dep['pkg']
            dep_kinds = [dk.get('kind') for dk in dep.get('dep_kinds', [])]
            # None = normal dependency, 'build' = build dependency
            if any(k in (None, 'build') for k in dep_kinds):
                if dep_id not in visited:
                    queue.append(dep_id)

    # `cargo metadata` resolves optional dependencies that no feature in
    # this build turns on (ring through rustls-webpki, say). `cargo tree`
    # follows the real feature resolution, so only what it lists is built.
    tree = subprocess.run(
        ['cargo', 'tree', '--locked', '-p', 'latch-host', '--target', target,
         '-e', 'normal,build', '--prefix', 'none', '--format', '{p}'],
        cwd=repo_root, capture_output=True, text=True, check=True,
    ).stdout
    built = set()
    for line in tree.splitlines():
        parts = line.split()
        if len(parts) >= 2:
            built.add((parts[0], parts[1].lstrip('v')))
    visited = {i for i in visited if (pkgs[i]['name'], pkgs[i]['version']) in built}

    # Inspect each reachable package
    target_findings = []
    for pkg_id in visited:
        pkg = pkgs[pkg_id]
        name = pkg['name']
        manifest_dir = os.path.dirname(pkg['manifest_path'])

        # Check build tool dependencies in resolved node
        build_deps = []
        node = nodes.get(pkg_id, {})
        for dep in node.get('deps', []):
            dep_kinds = [dk.get('kind') for dk in dep.get('dep_kinds', [])]
            if 'build' in dep_kinds:
                dep_pkg_name = pkgs.get(dep['pkg'], {}).get('name', '')
                if dep_pkg_name in BUILD_TOOL_NAMES or 'bindgen' in dep_pkg_name or 'autotools' in dep_pkg_name:
                    build_deps.append(dep_pkg_name)

        # Only build tools this build compiles count (resolved above);
        # an optional `bindgen` nobody enables runs nothing.
        build_deps = [d for d in build_deps if any(b[0] == d for b in built)]

        # Check native source files in manifest directory
        native_files = []
        if os.path.isdir(manifest_dir):
            for root, dirs, files in os.walk(manifest_dir):
                # Avoid scanning nested members or caches
                if '.git' in dirs:
                    dirs.remove('.git')
                if 'target' in dirs:
                    dirs.remove('target')
                for f in files:
                    if any(f.endswith(ext) for ext in NATIVE_EXTENSIONS):
                        rel_f = os.path.relpath(os.path.join(root, f), manifest_dir)
                        native_files.append(rel_f)

        # Check links key and pkg-config usage
        links = pkg.get('links')
        has_pkg_config = any(
            (d.get('name') == 'pkg-config' and d.get('kind') == 'build')
            for d in pkg.get('dependencies', [])
        ) or any(
            (pkgs.get(dep['pkg'], {}).get('name') == 'pkg-config' and 'build' in [dk.get('kind') for dk in dep.get('dep_kinds', [])])
            for dep in node.get('deps', [])
        )

        has_a = len(build_deps) > 0
        has_b = len(native_files) > 0
        has_c = links is not None

        if has_a or has_b or has_c:
            is_allowed = name in allowed
            requires_allow = has_a or has_b
            if requires_allow and not is_allowed:
                has_unallowed_violations = True

            target_findings.append({
                'name': name,
                'version': pkg['version'],
                'build_deps': sorted(set(build_deps)),
                'native_files_count': len(native_files),
                'sample_files': native_files[:2],
                'links': links,
                'has_pkg_config': has_pkg_config,
                'is_allowed': is_allowed,
                'requires_allow': requires_allow,
            })

    # Render target table
    if not target_findings:
        print("  No native build tools, sources, or links declared.")
        continue

    # Header
    print(f"{'Package':<24} {'Status':<10} {'Build Tools':<18} {'Native Files':<14} {'Links / System Lib'}")
    print("-" * 80)
    for f in sorted(target_findings, key=lambda x: x['name']):
        if f['requires_allow']:
            status = "ALLOWED" if f['is_allowed'] else "VIOLATION"
        else:
            status = "INFO (c)"

        btools = ",".join(f['build_deps']) if f['build_deps'] else "-"
        nfiles = f"{f['native_files_count']} files" if f['native_files_count'] > 0 else "-"
        
        links_desc = "-"
        if f['links']:
            links_desc = f"links={f['links']}"
            if f['has_pkg_config']:
                links_desc += " (pkg-config syslib)"
        elif f['has_pkg_config']:
            links_desc = "pkg-config"

        print(f"{f['name']:<24} {status:<10} {btools:<18} {nfiles:<14} {links_desc}")
        if f['sample_files']:
            samples = ", ".join(f['sample_files'])
            if f['native_files_count'] > 2:
                samples += f", ... (+{f['native_files_count'] - 2} more)"
            print(f"  └─ sources: {samples}")

print("\n" + "=" * 80)
if has_unallowed_violations:
    print("AUDIT FAILED: Unallowed packages with native build tools or sources detected.")
    print("Add verified exceptions to scripts/audit-native.allow if intentional.")
    sys.exit(1)
else:
    print("AUDIT PASSED: All packages comply with native code policy.")
    sys.exit(0)
EOF
