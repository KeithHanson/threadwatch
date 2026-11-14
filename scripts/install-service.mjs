import { readFileSync, writeFileSync, existsSync, realpathSync, statSync, chmodSync } from 'node:fs';
import { dirname, resolve, isAbsolute } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const marker = '# Managed by Threadwatch install-service.mjs';
const [role, environment, user] = process.argv.slice(2);
function quote(value) { return `"${value.replaceAll('\\', '\\\\').replaceAll('"', '\\"').replaceAll('%', '%%')}"`; }
try {
  if (!['server', 'collector'].includes(role) || !environment || !isAbsolute(environment) || !user || !/^[a-z_][a-z0-9_-]*[$]?$/.test(user)) {
    throw new Error('Usage: sudo /absolute/path/to/node scripts/install-service.mjs server|collector /absolute/path/to/service.env service-user');
  }
  if (process.getuid?.() !== 0) throw new Error('Installing a system service requires sudo.');
  const uid = Number(execFileSync('id', ['-u', user], { encoding: 'utf8' }).trim());
  if (uid === 0) throw new Error('Run Threadwatch as an unprivileged service user, not root.');
  const envFile = realpathSync(environment);
  const info = statSync(envFile);
  if (!info.isFile() || (info.mode & 0o077) !== 0 || info.uid !== uid) throw new Error('Environment file must be a mode-600 regular file owned by the service user.');
  const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
  const entry = resolve(root, role === 'server' ? 'server/index.mjs' : 'collector/index.mjs');
  if ([root, envFile, process.execPath].some(value => /[\r\n\0]/.test(value))) throw new Error('Service paths must not contain control characters.');
  if (!existsSync(entry)) throw new Error('Service entry point does not exist.');
  const name = role === 'server' ? 'threadwatch.service' : 'threadwatch-collector.service';
  const target = `/etc/systemd/system/${name}`;
  if (existsSync(target) && !readFileSync(target, 'utf8').startsWith(marker)) throw new Error('Refusing to replace a service not managed by Threadwatch.');
  const unit = `${marker}\n[Unit]\nDescription=Threadwatch ${role}\nWants=network-online.target\nAfter=network-online.target\n\n[Service]\nType=simple\nUser=${user}\nWorkingDirectory=${root.replaceAll('%', '%%')}\nExecStart=${quote(process.execPath)} ${quote(`--env-file=${envFile}`)} ${quote(entry)}\nRestart=on-failure\nRestartSec=5\nTimeoutStopSec=30\nUMask=0077\nNoNewPrivileges=true\nPrivateTmp=true\n\n[Install]\nWantedBy=multi-user.target\n`;
  writeFileSync(target, unit, { mode: 0o644 });
  chmodSync(target, 0o644);
  execFileSync('systemd-analyze', ['verify', target], { stdio: 'inherit' });
  execFileSync('systemctl', ['daemon-reload'], { stdio: 'inherit' });
  execFileSync('systemctl', ['enable', name], { stdio: 'inherit' });
  execFileSync('systemctl', ['restart', name], { stdio: 'inherit' });
  console.log(`Installed and enabled ${name}. Check with: systemctl status ${name}`);
} catch (error) {
  console.error(error.code ? `Service installation failed (${error.code}); inspect systemctl status for details.` : error.message);
  process.exitCode = 1;
}
