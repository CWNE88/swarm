# Swarm

Swarm is serverless, end-to-end encrypted communication: messages, posts,
group chats, file transfers, voice and video calls, conference calls,
screen sharing and remote control, going directly between people's
devices with nothing in between but the network.

There are no servers, accounts, usernames or phone numbers. Your identity
is a key you hold, restored on a new device from 12 words. It works best
over IPv6 and works over IPv4 too, including on an office network with no
internet connection at all.

## Features

- One identity on up to 8 devices: contacts see one person, messages reach
  every device and calls ring every one.
- Messages wait on your device and go by themselves when the contact comes
  online; messages can delete themselves after an hour, a day or a week.
- Group chats made from a chat by adding people, with no server holding
  them.
- Voice and video calls up to 4K, using the graphics chip where there is
  one; conference calls, voice and video.
- Screen sharing, and control of a shared desktop's mouse and keyboard
  with permission.
- File transfers of any size that carry on after a break.
- Cover mode, which hides when and how much you are communicating.
- Everything kept on disk is encrypted under your passphrase.
- A window, a terminal interface and a command line, with the same
  features.
- A single executable file on every computer, with nothing to install.

## Platforms

| Platform | File |
| --- | --- |
| Linux, x86-64 | `swarm` |
| Linux, 64-bit ARM (Raspberry Pi 4) | `swarm-arm` |
| Windows 10 and later | `swarm.exe` |
| Android 8 and later | `swarm.apk` |

On a computer, Swarm is one executable file: no installer, no
administrator rights, nothing added to the system, and it runs from any
folder or a USB stick. The window, the terminal interface and the command
line are all that one file. On Windows it needs nothing else at all; on
Linux it uses only the sound (ALSA) and Opus libraries that desktop
distributions already have, and glibc 2.36 or later (Debian 12, Ubuntu
22.10 and later). Your data lives in a `.swarm` folder in your home
folder.

Swarm uses UDP port 1977 by default.

## Installing

Download the file for your system from the releases page.

- **Windows** – Swarm isn't signed with a paid code-signing certificate,
  so Windows may say "Windows protected your PC" the first time it runs.
  Choose **More info**, then **Run anyway**.
- **Android** – Swarm isn't in Google Play. Open `swarm.apk` on the phone
  and allow installing from that app (usually your browser or file
  manager) when Android asks.
- **Linux** – make the file runnable (`chmod +x swarm`) and start it.

Each release has a `SHA256SUMS` file beside the downloads, listing the
SHA-256 checksum of every file. To check a download is the one published,
compare its line with what
`Get-FileHash swarm.exe` (PowerShell on Windows) or `sha256sum swarm`
(Linux) prints.

## Documentation

The manual is built into every version: the Help tab, or `swarm manual`.
Its source is [app/manual/manual.md](app/manual/manual.md), which includes
how the encryption works.

## Building

Swarm is written in Rust (edition 2024). On Debian 13:

    sudo apt install build-essential pkg-config cmake nasm clang \
        libasound2-dev libopus-dev libfontconfig-dev libxkbcommon-dev \
        libudev-dev libinput-dev libgbm-dev libdrm-dev libseat-dev libwayland-dev
    cargo build --release

The binary is `target/release/swarm`. `tools/build-all.sh` builds every
platform at once; what each cross build needs is at the top of
`tools/build-windows.sh`, `tools/build-android.sh` and
`.cargo/config.toml`.

Android builds are signed with a release key kept outside the repository
(`SWARM_RELEASE_KEYSTORE`); `SWARM_DEV_APK=1` builds a development copy
signed with the SDK's debug key instead.

## Tests

    tools/check.sh

runs every test, unoptimised and optimised, then clippy. Fuzz targets are
in `fuzz/` (see `fuzz/README`).

## Bug reports and contributions

Bug reports and suggestions are welcome as issues. Code contributions
(pull requests) aren't accepted: Swarm is written and maintained by one
person, and every change is fully tested before it goes in.

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability.

## Licence

Swarm is free software under the GNU General Public License, version 3
or (at your option) any later version: see [LICENCE](LICENCE). The patched
libraries in `vendor/` keep their own licences.
