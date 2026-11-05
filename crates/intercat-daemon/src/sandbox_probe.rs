//! Native probes for generated filesystem/network policies. The executable is
//! substituted through the same GitTool input; the policy is not weakened.
use crate::Result;
use intercat_git::WorktreeSource;
use std::{
    fs,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::Command,
};

pub(crate) struct Probe {
    root: tempfile::TempDir,
    pub program: PathBuf,
    pub exec_dir: PathBuf,
    socket: PathBuf,
    listener: UnixListener,
}

impl Probe {
    pub fn new() -> Result<Self> {
        let root = tempfile::Builder::new()
            .prefix("ic-rpc-policy-")
            .tempdir_in("/private/tmp")?;
        for path in [
            "job/tmp/exec",
            "job/repo",
            "job/control",
            "source/.git/objects",
            "run",
        ] {
            fs::create_dir_all(root.path().join(path))?;
        }
        let source = root.path().join("job/tmp/probe.c");
        let program = root.path().join("job/tmp/probe");
        let exec_dir = root.path().join("job/tmp/exec");
        fs::write(
            &source,
            r#"
#include <sys/socket.h>
#include <sys/un.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
int main(int argc, char **argv) {
  if (argc != 2) return 2;
  if (!strcmp(argv[1], "--exec-path")) {
    char path[4096]; strlcpy(path, argv[0], sizeof path);
    char *slash = strrchr(path, '/'); if (!slash) return 3;
    *slash = 0; printf("%s/exec\n", path); return 0;
  }
  struct sockaddr_un address = {0}; address.sun_family = AF_UNIX;
  if (strlen(argv[1]) >= sizeof address.sun_path) return 4;
  strlcpy(address.sun_path, argv[1], sizeof address.sun_path);
  int fd = socket(AF_UNIX, SOCK_STREAM, 0); if (fd < 0) return 5;
  int connected = connect(fd, (struct sockaddr *)&address, sizeof address) == 0;
  int error = connected ? 0 : errno; close(fd);
  printf("{\"connected\":%s,\"errno\":%d}\n", connected ? "true" : "false", error);
  return 0;
}
"#,
        )?;
        let built = Command::new("/usr/bin/clang")
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .output()?;
        if !built.status.success() {
            return Err(String::from_utf8_lossy(&built.stderr).into_owned().into());
        }
        let socket = root.path().join("run/rpc.sock");
        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            root,
            program,
            exec_dir,
            socket,
            listener,
        })
    }
    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }
    pub fn source(&self) -> WorktreeSource {
        WorktreeSource {
            root: self.path("source"),
            common_dir: self.path("source/.git"),
            head: "11".repeat(20).parse().unwrap(),
        }
    }
    pub fn check(&self, name: &str, profile: &str) -> Result<()> {
        let control = UnixStream::connect(&self.socket)?;
        self.listener.accept()?;
        drop(control);
        let output = Command::new("/usr/bin/sandbox-exec")
            .args(["-p", profile])
            .arg(&self.program)
            .arg(&self.socket)
            .current_dir(self.path("job/repo"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()?;
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            result,
            serde_json::json!({"connected":false,"errno":libc::EPERM}),
            "{name}"
        );
        assert!(
            matches!(self.listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        if let Some(root) = std::env::var_os("INTERCAT_RPC_POLICY_OUTPUT") {
            let root = PathBuf::from(root);
            fs::create_dir_all(&root)?;
            fs::write(root.join(format!("{name}.sb")), profile)?;
            fs::write(
                root.join(format!("{name}.json")),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"case":name,"positiveControl":true,"serverAccepted":false,"probe":result}),
                )?,
            )?;
        }
        Ok(())
    }
}
