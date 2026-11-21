{
  pkgs,
  module,
  package,
}:
let
  release_a = pkgs.symlinkJoin {
    name = "runboard-release-a";
    paths = [ package ];
  };
  release_b = pkgs.symlinkJoin {
    name = "runboard-release-b";
    paths = [ package ];
  };
  broken = pkgs.runCommand "runboard-broken-release" { } ''
    mkdir -p "$out/bin"
    ln -s ${pkgs.coreutils}/bin/false "$out/bin/runboard"
  '';
in
pkgs.testers.runNixOSTest {
  name = "runboard-deployment";
  nodes.machine =
    { lib, ... }:
    {
      imports = [ module ];
      services.runboard = {
        enable = true;
        inherit package;
        roots = [ "demo=/srv/demo" ];
        natsUrlFile = "/run/secrets/nats-url";
        deploy.enable = true;
      };
      # Exercise real activation and systemd sandboxing without fetching GitHub
      # or building remote revisions inside the guest.
      systemd.services.runboard-deploy.serviceConfig = {
        ExecCondition = lib.mkForce "";
        ExecStart = lib.mkForce "${pkgs.coreutils}/bin/true";
      };
      systemd.timers.runboard-deploy.wantedBy = lib.mkForce [ ];
      systemd.services.test-broker = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${pkgs.nats-server}/bin/nats-server --jetstream --addr 127.0.0.1 --port 4222 --user vm --pass synthetic-vm-password --store_dir /var/lib/test-broker";
          StateDirectory = "test-broker";
        };
      };
      environment.systemPackages = [ pkgs.curl ];
      virtualisation.memorySize = 1536;
    };

  testScript = ''
    start_all()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("test-broker.service")
    machine.succeed("install -d -m 755 /srv/demo/reports /run/secrets")
    machine.succeed("umask 077; printf '%s' 'nats://vm:synthetic-vm-password@127.0.0.1:4222' > /run/secrets/nats-url")
    machine.succeed("install -d -m 755 -o runboard-deploy -g runboard-deploy /var/lib/runboard-deploy")

    state = "/var/lib/runboard-deploy"
    revision_a = "a" * 40
    revision_b = "b" * 40
    revision_bad = "c" * 40

    def stage(revision: str, package: str) -> None:
        machine.succeed(f"ln -sfn {package} {state}/release-{revision}")
        machine.succeed(f"printf '%s' {revision} > {state}/target")

    def assert_release(revision: str, package: str) -> None:
        machine.succeed(f"test $(cat {state}/deployed) = {revision}")
        machine.succeed(f"test $(readlink {state}/current) = {package}")
        machine.wait_until_succeeds("curl --fail --silent http://127.0.0.1:5000/healthz | grep '\"live\"'", timeout=30)

    with subtest("first activation and private systemd credentials"):
        stage(revision_a, "${release_a}")
        machine.succeed("systemctl start runboard-deploy.service")
        assert_release(revision_a, "${release_a}")
        machine.succeed("curl --fail --silent http://127.0.0.1:5000/ > /dev/null")
        command = machine.succeed("systemctl show runboard.service --property=ExecStart --value")
        assert "synthetic-vm-password" not in command
        assert "--nats-url-file" in command
        process_command = machine.succeed("pid=$(systemctl show runboard.service -p MainPID --value); cat /proc/$pid/cmdline")
        assert "--nats-url-file" in process_command
        assert "synthetic-vm-password" not in process_command

    with subtest("staging leaves the active release unchanged"):
        stage(revision_b, "${release_b}")
        assert_release(revision_a, "${release_a}")
        machine.succeed("systemctl start runboard-deploy.service")
        assert_release(revision_b, "${release_b}")
        machine.succeed(f"test $(readlink {state}/previous) = ${release_a}")

    with subtest("interrupted activation retries restore the last healthy release"):
        stage(revision_bad, "${broken}")
        machine.succeed("systemctl start --no-block runboard-deploy.service")
        machine.wait_until_succeeds(f"test $(readlink {state}/current) = ${broken}", timeout=10)
        machine.succeed("systemctl kill --kill-whom=all --signal=KILL runboard-deploy.service")
        machine.wait_until_succeeds("systemctl is-failed runboard-deploy.service", timeout=10)
        machine.succeed(f"test $(readlink {state}/previous) = ${release_b}")
        machine.succeed("systemctl reset-failed runboard-deploy.service")
        machine.fail("systemctl start runboard-deploy.service")
        assert_release(revision_b, "${release_b}")
        machine.succeed(f"test $(readlink {state}/previous) = ${release_b}")

    with subtest("failed first activation stops rather than serving a broken release"):
        machine.succeed(f"rm {state}/deployed")
        machine.succeed("systemctl reset-failed runboard-deploy.service")
        machine.fail("systemctl start runboard-deploy.service")
        machine.fail("systemctl is-active runboard.service")
        machine.succeed(f"test ! -L {state}/current")
  '';
}
