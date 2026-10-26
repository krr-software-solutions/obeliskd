#!/bin/sh
# The release notes (markdown) for a version tag, shared by the GitLab and GitHub releases.
#
#   scripts/release-notes.sh v1.2.3
set -eu

tag="${1:?usage: scripts/release-notes.sh <tag>}"
version="${tag#v}"

cat <<MD
## obeliskd $version

**Install** (Linux, macOS; downloads the binary, then runs \`sudo obeliskd manage install\`):

\`\`\`sh
curl -fsSL https://github.com/krr-software-solutions/obeliskd/releases/download/$tag/install.sh | sh
\`\`\`

Or download the archive for your platform below.

**Crates** ([crates.io](https://crates.io/crates/obeliskd)):

\`\`\`sh
cargo install obeliskd-cli --version $version   # the \`obeliskd\` binary
cargo add obeliskd@$version                     # the library
\`\`\`
MD
