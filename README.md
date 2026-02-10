# Dropship

Ad-hoc local network file transfer server with an optional web UI.

**Warning**: serving a directory allows read and **write** access to that directory which can lead to security vulnerabilities. Purpose is to enable easier file sharing in safe local networks, not over public internet.

## Run


```bash
cargo run

# Or precompiled statically linked binaries:
# Mac / Linux
./dropship
# Windows
dropship.exe
```
Go to the url (default http://localhost:6767 ) and set the serving directory.

### Serve some directory on start without configuring first:

```bash
cargo run -- --dir /path/to/share

# Or precompiled statically linked binaries:
# Mac / Linux
./dropship --dir /path/to/share
# Windows
dropship.exe --dir /path/to/share
```

Optional token:

```bash
cargo run -- --dir /path/to/share --token mytoken
# Mac / Linux
./dropship --dir /path/to/share --token mytoken
# Windows
dropship.exe --dir /path/to/share --token mytoken
```

## Usage

- Open the printed localhost URL to access the admin dashboard. (default first try http://localhost:6767 )
- Set the serving directory
- Upload via drag-and-drop and download files or directories as archives (available to serving directory when set).
- Set or generate a token for remote access to the admin dashboard.
- Share the LAN URL with optional `?token=...` for remote users (admin).


