# Security policy

LinkCursor lets a connected device inject keyboard and mouse input and exchange clipboard contents and files, so security problems can have serious consequences. Thank you for reporting them responsibly.

## Reporting a vulnerability

**Do not open a public issue.** Instead, use [private vulnerability reporting](../../security/advisories/new) on this repository. If that is not available, email akarineren@gmail.com.

Please include:

- the affected repository and version or commit;
- the operating systems involved;
- steps to reproduce, and what an attacker gains;
- whether the attacker needs to be on the same network, or paired.

You can expect an acknowledgement within a week. This is a small project, so fixes may take longer; we will keep you informed and credit you in the release notes unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes. LinkCursor is at an early stage (0.x), and devices should run matching versions.

## What is in scope

- Connecting to, pairing with or controlling a device without the user's approval
- Impersonating a paired device, or defeating the six-digit pairing code
- Reading or changing traffic between devices
- Reaching files or clipboard contents that were not offered
- Privilege escalation through the Windows input helper and service

Known limits, such as discovery announcements being plain UDP and unpaired connections being allowed while `require_pairing` is off, are documented in the [core README](https://github.com/rinflow-dev/rinflow-core#security).
