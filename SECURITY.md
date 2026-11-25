# Security

Swarm's whole purpose is private communication, so security reports are
taken seriously and are welcome.

## Reporting a vulnerability

Please report privately, through GitHub: the repository's **Security**
tab, then **Report a vulnerability**. Don't open a public issue for a
security problem.

A useful report says which version and platform, what an attacker can do,
and how to reproduce it. You'll get an answer, and once a fix is out the
report is published with credit to you, unless you'd rather not be named.

## Supported versions

Only the latest version gets security fixes; there are no long-term
branches.

## In scope

- The cryptography and protocol: the Noise handshake, the encrypted
  streams, identity and device keys, signed records.
- Encrypted storage: anything readable or changeable in `~/.swarm/`
  without the passphrase.
- Anything a contact, a stranger, a group member or someone on the
  network path can make another device do: crashes, hangs, reading or
  changing data, running code, controlling a screen without permission.
- Metadata Swarm promises to hide, such as cover mode's traffic pattern.

## Out of scope

- A device that is already compromised, or unlocked in someone else's
  hands.
- What relaying necessarily sees: a device passing a conference call on
  sees what it passes on, as the manual explains.
- That your contacts learn your devices' IP addresses: connections are
  direct, so they have to.
