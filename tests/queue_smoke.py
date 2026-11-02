#!/usr/bin/env python3
"""Qualify the real pinned local broker with fake Telegram and yt-dlp only."""

import argparse
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import tempfile
import time

from smoke import smoke


class Broker:
    def __init__(self, binary, app, root, port):
        self.binary, self.root, self.port = binary, root, port
        self.process = None
        self.log = (root / 'broker.log').open('w+')
        self.env = {key: value for key, value in os.environ.items() if not key.startswith('IGGY_')}
        self.env.update(IGGY_CONFIG_PATH=str(Path(__file__).resolve().parents[1] / 'deploy/alpine/iggy-yt-dlp.toml'),
                        IGGY_PATH=str(root / 'broker'), IGGY_TCP_ADDRESS=f'127.0.0.1:{port}',
                        IGGY_ROOT_USERNAME='fixture_admin', IGGY_ROOT_PASSWORD=secrets.token_hex(24),
                        IGGY_ENV_PATH=str(root / 'empty.env'))
        (root / 'empty.env').write_text('')
        self.admin = root / 'admin-uri'
        self.connection = root / 'app-uri'
        self.admin.write_text(f"iggy://fixture_admin:{self.env['IGGY_ROOT_PASSWORD']}@127.0.0.1:{port}")
        self.connection.write_text(f'iggy://fixture_app:{secrets.token_hex(24)}@127.0.0.1:{port}')
        self.admin.chmod(0o600)
        self.connection.chmod(0o600)
        try:
            self.start()
            env = {key: value for key, value in os.environ.items() if not key.startswith('IGGY_')}
            env.update(IGGY_CONNECTION_STRING_FILE=str(self.admin),
                       IGGY_APP_CONNECTION_STRING_FILE=str(self.connection))
            subprocess.run([app, 'iggy-provision'], env=env, check=True, timeout=60)
        except BaseException:
            self.stop()
            self.log.close()
            raise
        self.env.pop('IGGY_ROOT_USERNAME')
        self.env.pop('IGGY_ROOT_PASSWORD')

    def start(self):
        assert self.process is None or self.process.poll() is not None
        self.process = subprocess.Popen([self.binary], env=self.env, cwd=self.root, stdout=self.log, stderr=self.log)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.seek(0)
                raise AssertionError('broker startup failed: ' + self.log.read())
            try:
                with socket.create_connection(('127.0.0.1', self.port), timeout=.2):
                    return
            except OSError:
                time.sleep(.1)
        raise AssertionError('broker listener did not start')

    def stop(self):
        if self.process is not None and self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            assert self.process.wait(timeout=45) == 0, 'broker did not stop cleanly'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--broker', required=True)
    parser.add_argument('--binary', default='target/debug/yt-dlp-tg')
    parser.add_argument('--test-binary', help='compiled Rust test executable; optional on SG')
    parser.add_argument('--trace', help='optional local strace output for native probe')
    parser.add_argument('--status-only', action='store_true', help='focused progress-edit regression')
    parser.add_argument('--progress-only', action='store_true', help='streamed two-phase progress and native AAC workflow')
    args = parser.parse_args()
    binary, server = str(Path(args.binary).resolve()), str(Path(args.broker).resolve())
    with tempfile.TemporaryDirectory(prefix='yt-dlp-tg-iggy-') as scratch:
        root = Path(scratch)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        broker = None
        try:
            broker = Broker(server, binary, root, port)
            queue_env = {'IGGY_CONNECTION_STRING_FILE': str(broker.connection),
                         'IGGY_STREAM': 'yt_dlp_tg', 'IGGY_TOPIC': 'downloads'}
            for mode in ('polling', 'webhook'):
                if args.progress_only:
                    smoke(binary, mode, queue_env=queue_env, progress_only=True)
                    continue
                if args.status_only:
                    smoke(binary, mode, queue_env=queue_env, status_only=True)
                    continue
                smoke(binary, mode, queue_env=queue_env, broker_control=broker)
                smoke(binary, mode, queue_env=queue_env, public_access=True)
                smoke(binary, mode, startup_stop=True, queue_env=queue_env)
                smoke(binary, mode, queue_env=queue_env, status_only=True)
            if args.test_binary:
                env = {**os.environ, 'IGGY_TEST_CONNECTION_FILE': str(broker.connection)}
                subprocess.run([str(Path(args.test_binary).resolve()), '--ignored', '--exact',
                                'queue::tests::real_broker_replay_and_active_drain', '--nocapture'],
                               env=env, check=True, timeout=120)
            broker.stop()
            broker.start()
            probe = [binary, 'iggy-probe']
            if args.trace:
                probe = ['strace', '-f', '-e', 'trace=io_uring_setup,epoll_create1', '-o',
                         str(Path(args.trace).resolve()), '--'] + probe
            subprocess.run(probe, env={**os.environ, **queue_env}, check=True, timeout=30)
            if args.trace:
                trace = Path(args.trace).read_text()
                assert 'io_uring_setup(' in trace and 'epoll_create1(' not in trace
                print('Native probe trace: io_uring setup observed; no epoll runtime setup')
            print('Real broker: persisted restart, restricted app access, queue workflows and outage recovery passed')
        finally:
            if broker:
                broker.stop()
                broker.log.close()


if __name__ == '__main__':
    main()
