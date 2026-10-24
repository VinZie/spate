# Security policy

## Supported versions

Spate is young, so only the latest release receives security fixes.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: open the repository's **Security** tab and choose **Report a vulnerability**. Don't open a public issue.

Include what an attacker can do, the steps to reproduce it and the Spate version. You should get a first reply within a week. Once a fix is released, you will be credited in the release notes unless you'd rather not be.

## What's in scope

Reports that are especially useful:

- A web page or another browser extension starting, changing or reading downloads.
- Getting past the IPC token in `ipc.json` or the native-messaging host's caller checks.
- Writing files outside the chosen download folder, for example through crafted file names or `Content-Disposition` headers.
- A server response that crashes Spate or corrupts other files on disk.

Out of scope: problems that need an attacker who already runs code as your Windows user, and the missing code signature (a known gap).
