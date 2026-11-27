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

## Out of scope: choices made on purpose

These are how Swarm is designed, not weaknesses. A report that Swarm
doesn't do what's said here is welcome; one that it shouldn't do it will
be answered with this list.

- A device that is already compromised, or unlocked in someone else's
  hands: anything running as you can read what Swarm can.
- What relaying necessarily sees: a device passing a conference call on
  sees what it passes on, as the manual explains.
- That your contacts learn your devices' IP addresses, all of them, local
  ones included: connections are direct, and Swarm has to work on a
  network with no internet.
- QUIC's TLS isn't what authenticates anyone, on purpose. It's transport
  only: a throwaway certificate nobody checks, and no server name (SNI)
  in its handshake, so nothing in the clear names who's being reached.
  The Noise handshake inside it proves who's at each end, and seals
  everything sent.
- Your devices share one user key. Removing a device can't take that key
  back from it; a lost device whose passphrase is known needs a new user
  key.
- No fingerprint or safety-number checks: a contact is the key you added,
  checked when you exchange it. There's no post-compromise security.
- A display name is only a label: two contacts may share one. The key is
  the contact.
- Contacts, and people in a group or call with you, are trusted with
  what they're sent, and could pass it on. Someone added to a group sees
  what's said from then on.
- A contact whose calls you've set to be answered without asking is
  trusted with that: if they bring you into a conference call, it's
  answered too, and the others in it hear you.
- Someone with a copy of your disk can't read or forge anything without
  your passphrase, but can delete files or put back older copies of
  them, as they could wipe the disk.
- Allowing someone to control your screen gives them control of it,
  Swarm's own settings included, until either of you stops it.
- In a conference, who may remove whom follows the order people came in.
  It's there to stop a newcomer removing those already in the call, not
  as a permission system; the worst it allows is removing someone, who
  can be rung back in.
