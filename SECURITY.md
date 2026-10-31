# Security

FREYM is development software. A passing build or fixture does not establish recording,
recovery or distribution acceptance. See [release gates](docs/RELEASE.md).

Report suspected vulnerabilities privately through the repository's GitHub
**Security → Advisories → Report a vulnerability** facility if available. If
private reporting is unavailable, ask the maintainer for a private channel
without publishing exploit details, recordings, access tokens or private paths.
Include the source revision, platform, reproduction steps and expected impact.
There is no promised response SLA or separate supported-version policy yet.

Recordings are local. Optional interaction metadata and extension permissions
must remain user controlled. Extension identity, payload hashes, capabilities,
per-call consent, current-session checks and visible agent control are enforced
by the native host. Desktop extensions run with the user's OS access and are not
sandboxed. Enable only reviewed packages. See [extensions](docs/EXTENSIONS.md).

Unsigned update payloads remain rejected; production update trust and signing
require owner configuration. Do not bypass fail-closed checks, load untrusted
native helpers or present an unsigned local executable as an accepted release.
Preserve safety comments and resource/thread ownership when reporting fixes.
