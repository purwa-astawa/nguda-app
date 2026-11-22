# nguda

nguda is a tray-first planner and time tracker that shows your real estimate gap. It plans your day from local tasks and Google Calendar, tracks the running task from the status bar, and lets you extend or switch when you run over. Today's summary puts planned time next to actual time, so you learn how far your estimates are off.

nguda runs on macOS and Linux.

## Install

Download the latest build from the [Releases page](https://github.com/purwa-astawa/nguda-app/releases).

### macOS

Download the `.dmg`. It is a universal build (Apple silicon and Intel) and needs macOS 13 or later.

The app is ad-hoc signed, not notarized by Apple, so Gatekeeper blocks it the first time you open it. To open it anyway:

1. Open the `.dmg` and drag nguda into Applications.
2. Open nguda once. macOS says it can't verify the app; click **Done** (or **Cancel**).
3. Open **System Settings → Privacy & Security**, scroll to the Security section, and click **Open Anyway** next to the message about nguda.
4. Confirm with **Open Anyway** and enter your password if asked.

After that, nguda opens normally.

### Linux

Builds are x86_64 only. Every package needs the Ayatana AppIndicator and WebKitGTK 4.1 runtime libraries; the `.deb` and `.rpm` declare them, so the package manager installs them for you.

- **AppImage:**

  ```sh
  chmod +x nguda_*.AppImage
  ./nguda_*.AppImage
  ```

- **Debian, Ubuntu** (`.deb`):

  ```sh
  sudo apt install ./nguda_*.deb
  ```

- **Fedora** (`.rpm`; on RHEL and its rebuilds, enable EPEL first, since `libayatana-appindicator-gtk3` is only there):

  ```sh
  sudo dnf install ./nguda-*.rpm
  ```

For the AppImage, install the runtime libraries yourself if they are missing (for example `libayatana-appindicator3-1` and `libwebkit2gtk-4.1-0` on Debian and Ubuntu).

### Launch at login

Launch at login is off by default. Turn on the **Launch at login** toggle in the nguda window to start it when you log in.

## The status indicator

One line of text describes what nguda is doing:

- `<task> · <elapsed>` while a task runs,
- `Next: <title>` while idle with something left to do,
- `nguda` otherwise.

On macOS that text sits next to the icon in the menu bar. On Linux, where it appears depends on your desktop:

| Desktop | Text next to the icon |
|---|---|
| GNOME | Yes, once the AppIndicator extension is installed (stock GNOME has no tray at all) |
| KDE Plasma | Probably not: icon only |
| Waybar, Xfce, Cinnamon | No: icon only |

On every desktop the same text is also the first line of the tray menu, so you can always see it there. Clicking the tray icon does nothing on Linux, so every action is a menu item:

- **Switch**, **Extend +5 min**, **Extend +15 min**, and **Extend…** (enabled while a task runs; Extend… opens the window at the running task's extend field),
- **Today's summary**, which opens the window on the Summary tab,
- **Quit**.

## Google Calendar

nguda reads your Google Calendar events into the day plan. Access is read-only.

### Sign in

In the window's Google Calendar section, click **Sign in**. Your browser opens Google's consent page; after you allow access, nguda lists your calendars and you choose which ones go into today's plan. **Sign out** forgets the sign-in but keeps every item already imported.

If the build has no built-in Google client (for example, when you built it yourself without one), **Sign in** is disabled with an explanation. Use your own client, below.

### Bring your own OAuth client

You can use your own Google OAuth client instead of the built-in one:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a project.
2. Enable the **Google Calendar API** for it.
3. Configure the OAuth consent screen (user type External is fine) and add these two scopes:
   - `https://www.googleapis.com/auth/calendar.events.readonly`
   - `https://www.googleapis.com/auth/calendar.calendarlist.readonly`
4. Add your own Google account as a test user. While the app stays in Testing mode, Google expires refresh tokens after 7 days, so you will need to sign in again about once a week.
5. Create an OAuth client ID of type **Desktop app**.
6. In nguda, while signed out, open the Google Calendar section, paste the **Client ID** (and the **Client secret**, which is optional) into the "Your own OAuth client" form, and click **Save**. **Clear** removes your client; nguda then uses the built-in client if the build has one, otherwise Sign in is disabled again.

Then click **Sign in**.

## Privacy

- Your plan, tasks, and time log are stored locally in a SQLite database, `nguda.sqlite3`:
  - macOS: `~/Library/Application Support/dev.nguda.app/`
  - Linux: `$XDG_DATA_HOME/dev.nguda.app/` (default `~/.local/share/dev.nguda.app/`)
- Google Calendar access uses read-only scopes (`calendar.events.readonly` and `calendar.calendarlist.readonly`). nguda never writes to your calendar.
- The Google refresh token is kept in the OS keyring (macOS Keychain, Linux Secret Service). If no keyring is available, the sign-in is kept in memory for the current session only.
- There is no telemetry. The only network traffic is to Google, for sign-in and calendar sync.

## Building from source

Prerequisites:

- Rust 1.99 or later
- Node.js 24
- On Linux, the system packages (Debian and Ubuntu names):

  ```sh
  sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev \
    libxdo-dev libssl-dev build-essential curl wget file
  ```

Then:

```sh
npm ci                  # install frontend dependencies
npm run tauri dev       # run in development
npm run tauri build     # build release bundles
npm test                # frontend tests
cargo test --workspace  # Rust tests
```

To build with a built-in Google client, set these at build time:

```sh
NGUDA_GOOGLE_CLIENT_ID=<client id> NGUDA_GOOGLE_CLIENT_SECRET=<client secret> npm run tauri build
```

Without `NGUDA_GOOGLE_CLIENT_ID`, the build has no built-in client and users must bring their own.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
