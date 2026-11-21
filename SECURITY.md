# Security policy

## Reporting a vulnerability

Report it privately through
[GitHub's private vulnerability reporting](https://github.com/Haibread/camembert/security/advisories/new).
The report opens a draft advisory that only the maintainers and you can see.

Do not open a public issue, pull request or discussion for a vulnerability:
described in public, it is disclosed before it is fixed.

A useful report holds:

- the affected version (`camembert --version`) and platform;
- the steps to reproduce it, with the smallest input that triggers it;
- the impact as you see it.

## Supported versions

Fixes ship in a new release; only the latest release is supported.

## Scope

In scope: the code in this repository and the artifacts a release publishes
(static binaries, the Windows archive, the `.deb` and `.rpm` packages and the
`PKGBUILD`). camembert reads untrusted input in several places, so these
count:

- memory-safety or logic flaws reached through a crafted `.cmbt` dump, an
  ncdu export passed to `camembert import`, or a hostile directory tree
  (names, symlinks, mount layout);
- the deletion path removing something other than what was marked;
- a release artifact that does not match its published checksum.

Out of scope:

- a vulnerability in a dependency that is already public upstream: report it
  there, Dependabot brings the fix in;
- a finding that needs an already-compromised host or account;
- scanner output with no path to exploitation.

## What happens next

Reports are handled on a best-effort basis.

1. The maintainers acknowledge the report in the draft advisory.
2. They assess it and ask for more detail if needed.
3. The fix is prepared in the advisory's private fork.
4. A release ships the fix.
5. The advisory is published, with a CVE requested through GitHub when the
   impact warrants one, and you are credited unless you prefer otherwise.
