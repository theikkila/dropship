# Dropship

Ad-hoc local network file transfer server with an optional web UI.

## Run

```bash
cargo run -- --dir /path/to/share
```

Optional token:

```bash
cargo run -- --dir /path/to/share --token mytoken
```

## Usage

- Open the printed localhost URL to access the admin dashboard.
- Set or generate a token for remote access.
- Share the LAN URL with optional `?token=...` for remote users.
- Upload via drag-and-drop and download files or directories as archives.
