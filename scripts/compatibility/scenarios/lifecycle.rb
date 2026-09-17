# frozen_string_literal: true
require "microsandbox"
require "json"
require "open3"

report = {status: "failed", passed: []}
begin
  name = ENV.fetch("MSB_COMPAT_EXISTING")
  2.times do
    sandbox = Microsandbox::Sandbox.start(name)
    output, status = Open3.capture2e(ENV.fetch("MSB_COMPAT_PYTHON"), ENV.fetch("MSB_COMPAT_VERIFY_RUNTIME"), name)
    raise output unless status.success?
    output = sandbox.exec("sh", ["-ec", 'test "$COMPAT_MARKER" = retained; test "$(cat /root/compat-marker)" = retained; test "$(stat -f -c %T /compat-data-0)" = tmpfs'])
    raise output.stderr unless output.success?
    sandbox.stop
  end
  report[:passed] << "existing/runtime-env-mounts-disk-restart"
  report[:status] = "passed"
ensure
  report[:error] = $!.message if $!
  File.write(ENV.fetch("MSB_COMPAT_REPORT"), JSON.generate(report))
end
