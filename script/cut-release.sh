#!/usr/bin/env bash
#
# Cut an Allele release: bump the version everywhere, roll the changelog,
# commit, and (optionally) tag. See RELEASING.md for the process this automates.
#
# Usage:
#   ./script/cut-release.sh minor              # 0.2.0 -> 0.3.0
#   ./script/cut-release.sh patch              # 0.2.0 -> 0.2.1
#   ./script/cut-release.sh 0.4.0              # explicit version
#   ./script/cut-release.sh minor --dry-run    # show the diff, change nothing
#   ./script/cut-release.sh minor --pr         # push a branch and open a PR
#   ./script/cut-release.sh minor --tag        # commit on master, tag, push
#
# Without --pr or --tag the script stops after the local commit and prints the
# remaining steps, so you can review before anything leaves the machine.
#
# The invariant from RELEASING.md — the tagged commit's Cargo.toml version equals
# the tag minus its "v" — is enforced here: --tag refuses to tag a commit whose
# Cargo.toml disagrees.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

die()  { echo "Error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

# ---------------------------------------------------------------- arguments --

BUMP=""
DRY_RUN=false
DO_PR=false
DO_TAG=false

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=true ;;
        --pr)      DO_PR=true ;;
        --tag)     DO_TAG=true ;;
        -h|--help) sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        -*)        die "unknown flag: $arg" ;;
        *)         [[ -n "$BUMP" ]] && die "unexpected argument: $arg"; BUMP="$arg" ;;
    esac
done

[[ -n "$BUMP" ]] || die "usage: $0 <major|minor|patch|X.Y.Z> [--dry-run] [--pr|--tag]"
$DO_PR && $DO_TAG && die "--pr and --tag are mutually exclusive"

# ------------------------------------------------------------ version maths --

CURRENT="$(perl -ne 'print $1 and last if /^version = "([^"]+)"/' Cargo.toml)"
[[ -n "$CURRENT" ]] || die "could not read version from Cargo.toml"

IFS=. read -r MAJOR MINOR PATCH <<<"$CURRENT"
case "$BUMP" in
    major)         VERSION="$((MAJOR + 1)).0.0" ;;
    minor)         VERSION="$MAJOR.$((MINOR + 1)).0" ;;
    patch)         VERSION="$MAJOR.$MINOR.$((PATCH + 1))" ;;
    [0-9]*.[0-9]*.[0-9]*) VERSION="$BUMP" ;;
    *)             die "not a bump keyword or X.Y.Z version: $BUMP" ;;
esac

TAG="v$VERSION"
info "$CURRENT -> $VERSION  (tag $TAG)"

# ------------------------------------------------------------- preflight -----

git rev-parse --git-dir >/dev/null 2>&1 || die "not a git repository"

if ! $DRY_RUN; then
    [[ -z "$(git status --porcelain)" ]] || die "working tree is dirty — commit or stash first"
fi

git fetch --tags --quiet origin || die "could not fetch from origin"
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null && die "tag $TAG already exists"

BRANCH="$(git branch --show-current)"
if $DO_TAG && [[ "$BRANCH" != "master" ]]; then
    die "--tag must run on master (currently on '$BRANCH'); use --pr from a branch"
fi
if [[ "$BRANCH" == "master" ]] && [[ "$(git rev-parse HEAD)" != "$(git rev-parse origin/master)" ]]; then
    die "master is not in sync with origin/master — pull or push first"
fi

# --------------------------------------------------------------- the bump ----

# Every edit goes through python3/perl rather than sed -i: BSD sed on macOS
# rejects the 0,/re/ address form and needs a different -i signature to GNU's.
TOUCHED=(Cargo.toml Cargo.lock resources/Info.plist CHANGELOG.md)

# Leave no half-applied bump behind if a later step bails out.
revert_on_failure() {
    local status=$?
    if (( status != 0 )); then
        git checkout -- "${TOUCHED[@]}" 2>/dev/null || true
        echo "==> reverted partial edits" >&2
    fi
    exit $status
}

bump_files() {
    trap revert_on_failure EXIT
    perl -0pi -e "s/^version = \"\Q$CURRENT\E\"/version = \"$VERSION\"/m" Cargo.toml
    perl -0pi -e "s/(name = \"allele\"\nversion = )\"\Q$CURRENT\E\"/\${1}\"$VERSION\"/" Cargo.lock
    perl -0pi -e "s{(<key>CFBundleShortVersionString</key>\s*\n\s*<string>)\Q$CURRENT\E(</string>)}{\${1}$VERSION\${2}}" resources/Info.plist

    VERSION="$VERSION" python3 - <<'PY'
import datetime, os, re

version = os.environ["VERSION"]
today = datetime.date.today().isoformat()
path = "CHANGELOG.md"
text = open(path).read()

start = text.index("## [Unreleased]")
end = re.search(r"^## \[\d", text[start + 1:], re.M)
if end is None:
    raise SystemExit("CHANGELOG.md: no released section after [Unreleased]")
end = start + 1 + end.start()
block = text[start:end]

# Collect the bullets under each "### Heading", merging duplicate headings that
# accumulate on master when several PRs each append their own "### Added".
groups = {}
for chunk in re.split(r"\n### ", block)[1:]:
    head, _, body = chunk.partition("\n")
    items, cur = [], None
    for line in body.split("\n"):
        if line.startswith("- "):
            if cur is not None:
                items.append(cur.rstrip())
            cur = line
        elif cur is not None and line.strip():
            cur += "\n" + line
    if cur is not None:
        items.append(cur.rstrip())
    groups.setdefault(head.strip(), []).extend(items)

if not any(groups.values()):
    raise SystemExit("CHANGELOG.md: [Unreleased] has no entries — nothing to release")

new = "## [Unreleased]\n\nChanges on `master` awaiting the next tagged release.\n\n"
new += f"## [{version}] - {today}\n\n"
for head in ("Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"):
    if groups.get(head):
        new += f"### {head}\n" + "\n".join(groups[head]) + "\n\n"
for head, items in groups.items():  # anything non-standard, kept rather than dropped
    if head not in ("Added", "Changed", "Deprecated", "Removed", "Fixed", "Security") and items:
        new += f"### {head}\n" + "\n".join(items) + "\n\n"

text = text[:start] + new + text[end:]

# Refresh the compare links at the foot of the file.
prev = re.search(r"^\[(\d+\.\d+\.\d+)\]: ", text[text.index("[Unreleased]: "):], re.M)
prev = prev.group(1) if prev else None
repo = "https://github.com/devergehq/allele"
text = re.sub(
    r"^\[Unreleased\]: .*$",
    f"[Unreleased]: {repo}/compare/v{version}...HEAD\n"
    f"[{version}]: {repo}/compare/v{prev}...v{version}" if prev else
    f"[Unreleased]: {repo}/compare/v{version}...HEAD\n"
    f"[{version}]: {repo}/releases/tag/v{version}",
    text, count=1, flags=re.M,
)

open(path, "w").write(text)
PY

    trap - EXIT
}

if $DRY_RUN; then
    info "dry run — showing the diff, then reverting"
    bump_files
    git --no-pager diff -- "${TOUCHED[@]}"
    git checkout -- "${TOUCHED[@]}"
    exit 0
fi

bump_files

# Cheap consistency check: cargo re-reads the manifest and lockfile together and
# fails loudly if the two versions disagree.
info "validating manifest and lockfile"
cargo metadata --no-deps --offline --format-version 1 >/dev/null

git add "${TOUCHED[@]}"
git commit --quiet -m "chore(release): $VERSION

Bump Cargo.toml, Cargo.lock and Info.plist to $VERSION, and move the
[Unreleased] entries into a dated [$VERSION] section."

info "committed $(git rev-parse --short HEAD)"

# --------------------------------------------------------------- publish -----

if $DO_PR; then
    command -v gh >/dev/null || die "--pr needs the gh CLI"
    [[ "$BRANCH" != "master" ]] || die "--pr needs a branch other than master"
    git push --quiet -u origin "$BRANCH"
    gh pr create --base master --head "$BRANCH" \
        --title "chore(release): $VERSION" \
        --body "Release-prep commit for **$TAG**, per RELEASING.md.

Merge this, then tag the merge commit:

\`\`\`sh
git checkout master && git pull
git tag $TAG && git push origin $TAG
\`\`\`"
    info "PR opened — merge it, then tag master with $TAG"
    exit 0
fi

if $DO_TAG; then
    TAGGED="$(perl -ne 'print $1 and last if /^version = "([^"]+)"/' Cargo.toml)"
    [[ "$TAGGED" == "$VERSION" ]] || die "refusing to tag: Cargo.toml says $TAGGED, tag is $TAG"
    git tag "$TAG"
    git push --quiet origin master
    git push origin "$TAG"
    info "pushed $TAG — release.yml is building; watch it with:"
    echo "    gh run watch \$(gh run list --workflow=release.yml -L1 --json databaseId -q '.[0].databaseId')"
    exit 0
fi

cat <<EOF

Local commit only. Next, either:

  # via a PR (matches how master is normally updated)
  git checkout -b release-$VERSION && git push -u origin release-$VERSION
  gh pr create --base master --title "chore(release): $VERSION" --fill
  # ...merge, then:
  git checkout master && git pull && git tag $TAG && git push origin $TAG

  # or directly, if you commit to master
  git push origin master && git tag $TAG && git push origin $TAG

Pushing the tag is the only trigger release.yml needs.
EOF
