# Debugging Rust apps

You can debug Rust applications running remotely over SSH using tools like
`gdb` (GNU Debugger) on the command line or an IDE like JetBrains RustRover.

## Debugging with GDB over SSH

To debug remotely using `gdb`, you'll need `gdb` installed on the remote
machine and your application compiled with debug symbols (`cargo build`).

1. **Connect via SSH:**
   Connect to your remote server where the application will run.

    ```bash
    ssh user@remote_host
    ```

2. **Start the application with GDB:**
   Navigate to your project directory on the remote machine and start `gdb`
   with your executable.

    **Important:** Run `gdb` as the `conduwuit` user (not `root`) to avoid
    mixed file ownership on your database `.sst` files, which risks forcing
    RocksDB to do extra work.

    ```bash
    sudo -u conduwuit gdb ./target/debug/continuwuity
    ```

3. **Set breakpoints:**
   Inside the `gdb` prompt, you can set breakpoints using the `break` or `b`
   command followed by the file and line number, or the function name.

    ```gdb
    (gdb) break src/main/main.rs:10
    (gdb) break continuwuity::main
    ```

4. **Run the application:**
   Start the execution with the `run` or `r` command. You should pass the
   path to your configuration file here using the `--config` flag or by setting
   the `CONDUWUIT_CONFIG` environment variable beforehand.

    ```gdb
    (gdb) run --config /path/to/conduwuit.toml
    ```

5. **Debugging commands:**
    - `continue` or `c`: Continue execution until the next breakpoint.
    - `next` or `n`: Step over the current line of code.
    - `step` or `s`: Step into the current function.
    - `print <variable>` or `p <variable>`: Print the value of a variable.
    - `backtrace` or `bt`: Show the current call stack.

## Debugging with RustRover over SSH

JetBrains RustRover provides remote development capabilities, allowing you to
develop and debug on a remote server. You can either deploy the full remote
IDE backend or use a lightweight approach connecting locally to `gdbserver`
running on the remote machine.

### Option 1: Lightweight Remote Debugging (gdbserver)

This approach avoids installing the full IDE backend on the remote server by
connecting your local IDE to a remote `gdbserver` instance. You must have an
identical local build of your project to provide the debug symbols.

1. **Start gdbserver (Remote):** SSH into your server, navigate to the project,
   and start `gdbserver`. Remember to run this as the `conduwuit` user and pass
   the `--config` flag to your application.

    ```bash
    sudo -u conduwuit gdbserver :1234 ./target/debug/continuwuity --config /path/to/conduwuit.toml
    ```

2. **Configure RustRover (Local):** Open your local project. Go to
   "Run | Edit Configurations..." and add a **Remote Debug** configuration.
    - **Debugger:** Select `GDB`.
    - **'target remote' args:** Enter `remote_ip:1234`. If using an SSH tunnel
      (e.g., `ssh -L 1234:localhost:1234 user@remote_ip`), enter
      `localhost:1234`.
    - **Symbol file:** Select the local path to your compiled debug binary
      (`target/debug/continuwuity`).
    - **Path Mappings:** Add mapping if local and remote source paths differ.

3. **Debug:** Set breakpoints locally, select the new Remote Debug
   configuration, and click the "Debug" icon to attach.

### Option 2: Full Remote Development

This approach installs the IDE backend on the remote server and handles
compilation and debugging transparently.

1. **Connect:** Use the "Remote Development" tab on the Welcome screen to
   connect via SSH and open your project directory (`/path/to/continuwuity`).
2. **Configure Run/Debug:** Open "Edit Configurations..." and add a Cargo
   configuration. Remember to pass the server configuration file. For example,
   set the command to `run --bin continuwuity -- --config /path/to/conduwuit.toml`
   or set the `CONDUWUIT_CONFIG` environment variable.
3. **Debug:** Set breakpoints by clicking the left gutter, then click the
   "Debug" icon (bug symbol) next to your configuration. The IDE will compile,
   launch remotely, and automatically attach the debugger.
