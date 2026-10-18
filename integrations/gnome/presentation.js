// SPDX-License-Identifier: AGPL-3.0-or-later
// M0 presentation probe. This is trusted repository code, never wallpaper code.
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';

const START_TIMEOUT_MS = 10000;
const STOP_TIMEOUT_MS = 1500;
const LOG_LIMIT = 16384;

/** One external worker presented inside an existing GNOME background actor.
 *
 * This probe deliberately does not patch app lists, overview or workspace methods.
 * It is not an installable extension or a supported Apply backend yet.
 */
export class PresentationProbe {
    constructor() {
        this._run = null;
        this._state = 'idle';
        this._diagnostics = '';
        this._generation = 0;
    }

    status() {
        const run = this._run;
        return {
            state: this._state,
            pid: run?.process.get_identifier() ?? null,
            hasClone: !!run?.clone,
            minimized: run?.window?.minimized ?? false,
            ownsWindow: !!run?.window && run.client.owns_window(run.window),
            diagnostics: this._diagnostics,
        };
    }

    async start(executable, source, environment = {}) {
        const generation = ++this._generation;
        await this._stop();
        if (generation !== this._generation) return;
        if (typeof Meta.WaylandClient.new_subprocess !== 'function')
            throw new Error('This probe requires GNOME 49 Wayland client APIs');
        if (!GLib.path_is_absolute(executable) || !GLib.path_is_absolute(source))
            throw new Error('The probe requires absolute executable and content paths');
        if (!GLib.file_test(executable, GLib.FileTest.IS_REGULAR)
            || !GLib.file_test(executable, GLib.FileTest.IS_EXECUTABLE)
            || !GLib.file_test(source, GLib.FileTest.EXISTS))
            throw new Error('The worker executable or content path is unavailable');
        if (!Main.layoutManager._bgManagers?.[Main.layoutManager.primaryIndex]?.backgroundActor)
            throw new Error('The primary desktop background is unavailable');

        const run = {client: null, process: null, window: null, clone: null,
            mapSignal: 0, unmanagedSignal: 0, startupTimer: 0, stopTimer: 0, stopping: false};
        this._run = run;
        this._state = 'starting';
        this._diagnostics = '';
        run.mapSignal = global.window_manager.connect_after('map', (_wm, actor) => {
            const window = actor.meta_window;
            if (this._run !== run || !window || !run.client?.owns_window(window) || run.stopping)
                return;
            if (run.window) {
                window.delete(global.get_current_time());
                return;
            }
            run.window = window;
            // Wait for map before managing fullscreen: the initial XDG configure
            // belongs to the client/compositor handshake, not this presentation shim.
            window.make_fullscreen();
            window.minimize();
            // Startup and wallpaper transitions may replace the manager while the
            // worker is starting. Resolve the current actor at map time.
            const background = Main.layoutManager._bgManagers?.[Main.layoutManager.primaryIndex];
            if (!background?.backgroundActor) {
                this.stop().catch(error => console.error(error));
                return;
            }
            const monitor = Main.layoutManager.primaryMonitor;
            const clone = new Clutter.Clone({source: actor, reactive: false});
            run.clone = clone;
            clone.set_size(monitor.width, monitor.height);
            clone.connect('destroy', () => {
                if (this._run === run && run.clone === clone) {
                    run.clone = null;
                    this.stop().catch(error => console.error(error));
                }
            });
            background.backgroundActor.add_child(clone);
            run.unmanagedSignal = window.connect('unmanaged', () => this._detach(run));
            if (run.startupTimer) GLib.source_remove(run.startupTimer);
            run.startupTimer = 0;
            this._state = 'mapped';
        });

        const launcher = new Gio.SubprocessLauncher({
            flags: Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_MERGE,
        });
        try {
            for (const [name, value] of Object.entries(environment))
                launcher.setenv(name, value, true);
            run.client = Meta.WaylandClient.new_subprocess(global.context, launcher,
                [executable, 'preview', source]);
            run.process = run.client.get_subprocess();
        } catch (error) {
            this._release(run);
            this._run = null;
            this._state = 'failed';
            throw error;
        } finally {
            // The launcher retains the child's socket peer until explicitly closed.
            // Leaving it to GC prevents EOF/window cleanup after the worker exits.
            launcher.close();
        }

        const decoder = new TextDecoder();
        const stream = run.process.get_stdout_pipe();
        const read = () => stream.read_bytes_async(4096, GLib.PRIORITY_DEFAULT, null, (input, result) => {
            try {
                const bytes = input.read_bytes_finish(result).get_data();
                if (!bytes?.length) {
                    input.close(null);
                    return;
                }
                if (this._run === run) {
                    // GJS's TextDecoder has no streaming option. A split UTF-8
                    // character may be replaced in diagnostics; byte reads stay bounded.
                    this._diagnostics = (this._diagnostics + decoder.decode(bytes)).slice(-LOG_LIMIT);
                }
                read();
            } catch (error) {
                if (this._run === run)
                    this._diagnostics = String(error).slice(-LOG_LIMIT);
            }
        });
        read();
        run.done = new Promise(resolve => {
            run.process.wait_async(null, (process, result) => {
                process.wait_finish(result);
                this._release(run);
                if (this._run === run) {
                    this._run = null;
                    this._state = run.stopping ? 'idle' : 'exited';
                }
                resolve();
            });
        });
        run.startupTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, START_TIMEOUT_MS, () => {
            run.startupTimer = 0;
            this._state = 'startup-timeout';
            this.stop().catch(error => console.error(error));
            return GLib.SOURCE_REMOVE;
        });
    }

    _detach(run) {
        if (run.unmanagedSignal) run.window.disconnect(run.unmanagedSignal);
        run.unmanagedSignal = 0;
        const clone = run.clone;
        run.clone = null;
        clone?.destroy();
        run.window = null;
    }

    _release(run) {
        this._detach(run);
        if (run.mapSignal) global.window_manager.disconnect(run.mapSignal);
        run.mapSignal = 0;
        for (const timer of ['startupTimer', 'stopTimer']) {
            if (run[timer]) GLib.source_remove(run[timer]);
            run[timer] = 0;
        }
    }

    stop() {
        ++this._generation;
        return this._stop();
    }

    async _stop() {
        const run = this._run;
        if (!run) return;
        if (!run.stopping) {
            run.stopping = true;
            this._detach(run);
            run.process.send_signal(2);
            run.stopTimer = GLib.timeout_add(GLib.PRIORITY_DEFAULT, STOP_TIMEOUT_MS, () => {
                run.stopTimer = 0;
                run.process.force_exit();
                return GLib.SOURCE_REMOVE;
            });
        }
        await run.done;
    }
}
