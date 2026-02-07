# schengen-client

`schengen-client` is a commandline utility that connects to a
Synergy/Deskflow-compatible server. Input received from that server is
forwarded via the RemoteDesktop portal and libei to the compositor.

This utility only supports Linux and only supports compositors that use the
portals. There are no plans to support Xorg/X11-based services and it's very
unlikely that Macos or Windows support will ever appear.

This crate is part of the schengen project:
- [schengen](https://github.com/schengen-project/schengen) for the protocol implementation
- [schengen-server](https://github.com/schengen-project/schengen-server) for a synergy-compatible server
- [schengen-client](https://github.com/schengen-project/schengen-client) for a client that can connect to this server
- [schengen-debugger](https://github.com/schengen-project/schengen-debugger) for a protocol debugger


## Building and Installing

This is a typical Rust crate, see the Rust documentation and tutorials for any
questions.

Build with
```
$ cargo build
$ cargo install
```

Run the client with the server's IP or hostname as argument (default port:
24801)
```
$ schengen-client --verbose 192.168.1.100
```

## Activation via systemd

`schengen-client` supports socket-activation. This requires a server that
reaches out to the clients (typically the clients connect to the server
instead).

For configuration, modify the `.service` and `.socket` files in this
repository. Modify them as needed, copy them to
`~/.config/systemd/user/` and enable them with:

```console
$ systemctl --user enable --now schengen-client.service
```
Then connect and rejoice at your session being taken over.

## License

GPLv3 or later
