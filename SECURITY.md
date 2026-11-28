# Security

ScarLight runs code from files you supply, so a crash is often not a bug you can fix by design.
If you find something that is a real security problem (memory-safety issue reachable from
untrusted input, path traversal in the loader or a parser, a way to make the build or tests run
foreign code, a leaked token in the history), please report it **privately**:

**Security → Report a vulnerability** on
<https://github.com/pablogonz12/Scarlight/security/advisories/new> (GitHub private security
advisories). Do not open a public issue for it.

Include the commit, your platform, and the smallest input that triggers it. Please do not include
game files, executable dumps or hashes of game data in the report: we cannot accept them
([docs/asset-policy.md](docs/asset-policy.md)).

Ordinary crashes, hangs and black frames are not security reports; use the bug report form.
