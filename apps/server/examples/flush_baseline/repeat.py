"""Run independent alternating release processes; retain samples, identities and exit codes."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[4]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('--ticks', type=int, default=30)
    parser.add_argument('--warmup', type=int, default=5)
    args = parser.parse_args()
    if not 3 <= args.rounds <= 20 or not 1 <= args.ticks <= 1000 or not 0 <= args.warmup <= 100:
        parser.error('rounds=3..20 ticks=1..1000 warmup=0..100')
    os.chdir(ROOT)
    if args.output.exists() or args.work.exists():
        parser.error('output and work must not exist')
    # Build before starting any measured process. Cargo metadata finds custom target directories.
    build = ['cargo', 'build', '--release', '--locked', '-p', 'torse-server', '--example', 'flush_baseline']
    subprocess.run(build, check=True)
    target = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--format-version', '1']))['target_directory']
    exe = Path(target) / 'release/examples' / ('flush_baseline.exe' if os.name == 'nt' else 'flush_baseline')
    args.output.mkdir(parents=True)
    args.work.mkdir(parents=True)
    environment = dict(format=1, revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
                       source_status=subprocess.check_output(['git', 'status', '--porcelain'], text=True).splitlines(),
                       rustc=subprocess.check_output(['rustc', '--version'], text=True).strip(),
                       platform=platform.platform(), logical_cpus=os.cpu_count(),
                       processor=platform.processor(),
                       executable_sha256=digest(exe), build_command=build, argv=sys.argv,
                       instrument_scope='example-local unsegmented file adapter; production Journal and ExecutionHost',
                       cache='not evicted', affinity='inherited', power_and_temperature='uncontrolled; not sampled')
    (args.output / 'environment.json').write_text(json.dumps(environment, indent=2), encoding='utf-8')
    summaries = []
    references = {}
    failed = False
    for round_number in range(args.rounds):
        backends = ['memory', 'file'] if round_number % 2 == 0 else ['file', 'memory']
        modes = [True, False] if round_number % 2 == 0 else [False, True]
        for enabled in modes:
            for backend in backends:
                name = f'run-{round_number + 1:02}-{backend}-' + ('on' if enabled else 'off')
                output = args.output / name
                output.mkdir()
                work = args.work / name
                command = [str(exe), '--output', str(work), '--backend', backend, '--ticks', str(args.ticks), '--warmup', str(args.warmup)]
                if not enabled:
                    command.append('--unmeasured')
                try:
                    result = subprocess.run(command, capture_output=True, timeout=600)
                except subprocess.TimeoutExpired as error:
                    result = subprocess.CompletedProcess(command, 124, error.stdout or b'',
                                                         (error.stderr or b'') + b'\nmeasurement exceeded 600 seconds\n')
                (output / 'stdout.txt').write_bytes(result.stdout)
                (output / 'stderr.txt').write_bytes(result.stderr)
                identity = dict(command=['release/examples/flush_baseline' + ('.exe' if os.name == 'nt' else '')] + command[1:],
                                exit_code=result.returncode, executable_sha256=digest(exe), states={})
                failed |= result.returncode != 0
                if result.returncode == 0:
                    shutil.copyfile(work / 'ticks.csv', output / 'ticks.csv')
                    with (work / 'ticks.csv').open(newline='', encoding='utf-8') as source:
                        rows = list(csv.DictReader(source))
                    for case in ('empty', 'light', 'entities'):
                        state = digest(work / f'{case}-state.bin')
                        identity['states'][case] = state
                        references.setdefault(case, state)
                        if references[case] != state:
                            failed = True
                        if backend == 'file':
                            journal = digest(work / case / 'journal.log')
                            identity.setdefault('journals', {})[case] = journal
                            references.setdefault(('journal', case), journal)
                            if references[('journal', case)] != journal:
                                failed = True
                        samples = [row for row in rows if row['case'] == case]
                        assert len(samples) == args.ticks
                        times = sorted(int(row['tick_ns']) for row in samples)
                        # Nearest-rank percentiles, per independent process, never pooled.
                        summaries.append(dict(run=name, case=case, measured=enabled, backend=backend,
                            tick_mean_ns=sum(times) / len(times), tick_p50_ns=times[(len(times)*50+99)//100-1],
                            tick_p95_ns=times[(len(times)*95+99)//100-1], tick_max_ns=max(times),
                            **{key: sum(int(row[key]) for row in samples) / len(samples) if enabled else None for key in
                               ('flush_requests', 'durable_hits', 'backend_sync_calls', 'file_sync_all_calls', 'append_records', 'append_bytes', 'sync_ns', 'append_ns', 'flush_ns')}))
                (output / 'process.json').write_text(json.dumps(identity, indent=2), encoding='utf-8')
                print(name, result.returncode, flush=True)
    if summaries:
        with (args.output / 'summary.csv').open('w', newline='', encoding='utf-8') as target:
            writer = csv.DictWriter(target, fieldnames=list(summaries[0]))
            writer.writeheader()
            writer.writerows(summaries)
    if failed:
        raise SystemExit('a process failed or final states differ; inspect raw records')


if __name__ == '__main__':
    main()
