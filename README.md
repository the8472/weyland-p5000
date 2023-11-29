
# Weyland P-5000 Work Loader

For lifting heavy wayland loads.

Status: A quick hack demonstrating wayland message proxying in userspace. Its error handling is shoddy and it leaves dead sockets behind on panic. But it manages to keep FF alive.

To run firefox under the wayland proxy:

```
cargo +nightly build --release
./target/release/p5wl firefox
```