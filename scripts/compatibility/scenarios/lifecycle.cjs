// Use only APIs present in both 0.6.18 and the candidate. Cleanup is owned by the driver.
const {execFileSync} = require('node:child_process');
const {writeFileSync} = require('node:fs');
const {pathToFileURL} = require('node:url');
async function main() {
  const report = {status: 'failed', passed: []};
  const existing = process.env.MSB_COMPAT_EXISTING;
  try {
    const {Sandbox} = await import(pathToFileURL(require.resolve('microsandbox')).href);
    for (const count of existing ? [1] : [0, 1, 3]) {
      const name = existing || `compat-common-${count}`;
      let builder = Sandbox.builder(name).image(process.env.MSB_COMPAT_IMAGE)
        .memory(256).cpus(1).env('COMPAT_MARKER', 'retained');
      for (let i = 0; i < count; i++) builder = builder.volume(`/compat-data-${i}`, v => v.tmpfs().size(8));
      let sandbox = existing ? await Sandbox.start(name) : await builder.create();
      for (let restart = 0; restart < 2; restart++) {
        execFileSync(process.env.MSB_COMPAT_PYTHON, [process.env.MSB_COMPAT_VERIFY_RUNTIME, name]);
        let script = 'test "$COMPAT_MARKER" = retained; ';
        if (!existing && !restart) script += 'printf retained > /root/compat-marker; ';
        script += 'test "$(cat /root/compat-marker)" = retained; ';
        for (let i = 0; i < count; i++) script += `test "$(stat -f -c %T /compat-data-${i})" = tmpfs; `;
        const result = await sandbox.exec('sh', ['-ec', script]);
        if (!result.success) throw Error(result.stderr());
        await sandbox.stop();
        if (!restart) sandbox = await Sandbox.start(name);
      }
      report.passed.push(`${name}/runtime-env-mounts-disk-restart`);
      if (!existing) await Sandbox.remove(name);
    }
    report.status = 'passed';
  } catch (error) { report.error = String(error); throw error; }
  finally { writeFileSync(process.env.MSB_COMPAT_REPORT, JSON.stringify(report)); }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
