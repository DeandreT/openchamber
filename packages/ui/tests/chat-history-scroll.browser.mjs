// Run with: bun packages/ui/tests/chat-history-scroll.browser.mjs (optional CHROME_PATH)
// Uses production React and the installed LegendList in a real Chromium page.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import {
    CdpClient, createPageTarget, evaluateValue, launchChrome, reservePort, resolveChrome,
    resolveProfileDir, stopChrome, wait,
} from '../../../scripts/perf/cdp.mjs';

const repo = fileURLToPath(new URL('../../../', import.meta.url));
const source = readFileSync(join(repo, 'packages/ui/src/components/chat/MessageList.tsx'), 'utf8');
const start = source.indexOf('type TimelineListProps =');
const end = source.indexOf("TimelineList.displayName = 'TimelineList';", start);
assert.ok(start >= 0 && end > start, 'Locate the production TimelineList component');
const estimatedSize = source.match(/^const TIMELINE_ESTIMATED_ENTRY_SIZE = .+;$/m)?.[0];
assert.ok(estimatedSize, 'Use the production row size estimate');
// Resolve the list the UI package actually installs; a stale hoisted copy at
// the repository root would otherwise be measured instead.
const legendList = join(
    dirname(createRequire(join(repo, 'packages/ui/package.json')).resolve('@legendapp/list/react')),
    'react.mjs',
);
// Prepend compensation loses sub-pixel remainders when row estimates are
// fractional, and whether they are depends on the measured average. One
// height profile can pass by luck, so the scenarios run over several.
const heightProfiles = [[5, 137, 180], [7, 137, 180], [3, 101, 180], [6, 53, 180], [4, 211, 180], [1, 0, 40], [1, 0, 2400]];

// Exercise the actual list component and its prop wiring without mounting the
// application or connecting to a user's backend. Only row content and the
// surrounding chat callbacks are supplied by the fixture.
const fixture = `
import React from ${JSON.stringify(join(repo, 'node_modules/react/index.js'))};
import { createRoot } from ${JSON.stringify(join(repo, 'node_modules/react-dom/client.js'))};
import { LegendList } from ${JSON.stringify(legendList)};
${estimatedSize}
const TimelineRowContext = React.createContext(null);
const useUIStore = () => true;
const resolveTimelineIsAtEnd = () => false;
const timelineKeyExtractor = item => item.key;
const timelineItemType = item => item.kind;
const mountedRows = new Set();
function Row({ item }) {
    React.useLayoutEffect(() => { mountedRows.add(item.key); }, [item.key]);
    const [settled, setSettled] = React.useState(!item.older);
    React.useEffect(() => {
        if (settled) return;
        const timer = setTimeout(() => setSettled(true), 180);
        return () => clearTimeout(timer);
    }, [settled]);
    return <div data-probe-id={item.key} style={{ height: settled ? item.height : 90 }}>
        Row {item.key}
    </div>;
}
const renderTimelineItem = ({ item }) => <Row item={item} />;
${source.slice(start, end)}
const frames = async count => {
    for (let i = 0; i < count; i++) await new Promise(requestAnimationFrame);
};
const params = new URLSearchParams(location.search);
const heightPeriod = Number(params.get('period'));
const heightStep = Number(params.get('step'));
const baseHeight = Number(params.get('base'));
document.documentElement.dir = params.get('dir');
const rowsFrom = (from, count, older = false) => Array.from({ length: count }, (_, i) => ({
    key: 'row-' + (from + i), kind: 'turn', height: baseHeight + ((from + i + 100) % heightPeriod) * heightStep, older,
}));
const noop = () => {};
let list;
const registerList = value => { list = value; };
function App() {
    const [rows, setRows] = React.useState(() => rowsFrom(20, 60));
    const [footerHeight, setFooterHeight] = React.useState(50);
    window.runScenario = async (moveWhileLoading) => {
        await frames(40);
        const scroller = list.getScrollableNode();
        const initialTail = scroller.querySelector('[data-probe-id="row-79"]');
        const initialEndGap = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight;
        const initialMountCount = mountedRows.size;
        const initialRows = [...scroller.querySelectorAll('[data-probe-id]')];
        const initialViewportCovered = initialRows.some(node => {
            const rect = node.getBoundingClientRect();
            const viewport = scroller.getBoundingClientRect();
            return rect.top <= viewport.top + 1 && rect.bottom > viewport.top;
        });
        const moveTo = top => {
            scroller.dispatchEvent(new WheelEvent('wheel', { deltaY: -500, bubbles: true }));
            scroller.scrollTop = top;
        };
        moveTo(moveWhileLoading ? 600 : 0);
        await frames(40);
        // The request begins here. The reader can keep scrolling before the
        // page arrives; preservation must use their latest visible position.
        if (moveWhileLoading) {
            await new Promise(resolve => setTimeout(resolve, 80));
            moveTo(100);
            await frames(10);
        }
        const visible = [...scroller.querySelectorAll('[data-probe-id]')].find(
            node => node.getBoundingClientRect().bottom > scroller.getBoundingClientRect().top + 1,
        );
        if (!visible) throw new Error('No visible anchor');
        const key = visible.dataset.probeId;
        const read = () => {
            const node = scroller.querySelector('[data-probe-id="' + key + '"]');
            return node ? node.getBoundingClientRect().top - scroller.getBoundingClientRect().top : null;
        };
        const before = read();
        const samples = [];
        setRows(current => [...rowsFrom(0, 20, true), ...current]);
        const started = performance.now();
        for (let i = 0; i < 70; i++) {
            await frames(1);
            // Read after the frame's layout/ResizeObserver work, not halfway
            // through the pre-paint measurement and scroll compensation pass.
            await new Promise(resolve => setTimeout(resolve, 0));
            samples.push(read());
        }
        const elapsed = performance.now() - started;
        const after = read();
        setFooterHeight(500);
        setRows(current => [...current, ...rowsFrom(80, 1)]);
        await frames(20);
        const tailGrowthShift = read() - after;
        return {
            initialTailMounted: Boolean(initialTail), initialEndGap, initialViewportCovered, initialMountCount,
            key, before, after, elapsed, frameCount: samples.length,
            dataCount: list.getState().data.length,
            missingFrames: samples.filter(value => value === null).length,
            maxShift: Math.max(...samples.filter(value => value !== null).map(value => Math.abs(value - before))),
            tailGrowthShift,
        };
    };
    return <TimelineList
        entries={rows} registerList={registerList} endPinningReleased={true}
        composerOverlayHeight={0} onIsAtEndChange={noop} onListMetricsChange={noop}
        onTimelineDataChange={noop} rowContext={{ sessionIsWorking: false }}
        listFooter={<div style={{ height: footerHeight }} />}
        scrollContainerProps={{ style: { height: 600, width: 800, overflow: 'auto', overflowAnchor: 'none' } }}
    />;
}
createRoot(document.getElementById('root')).render(<App />);
`;

const build = await Bun.build({
    entrypoints: ['history-fixture'],
    minify: true,
    define: { 'process.env.NODE_ENV': '"production"' },
    plugins: [{
        name: 'history-fixture',
        setup(builder) {
            builder.onResolve({ filter: /^history-fixture$/ }, () => ({ path: 'entry', namespace: 'fixture' }));
            builder.onLoad({ filter: /.*/, namespace: 'fixture' }, () => ({ contents: fixture, loader: 'tsx' }));
            // React 19 provides the hook that the compatibility shim delegates to.
            builder.onResolve({ filter: /^use-sync-external-store\/shim$/ }, () => ({ path: 'shim', namespace: 'react-shim' }));
            builder.onLoad({ filter: /.*/, namespace: 'react-shim' }, () => ({
                contents: `export { useSyncExternalStore } from ${JSON.stringify(join(repo, 'node_modules/react/index.js'))};`,
                loader: 'js',
            }));
        },
    }],
});
assert.ok(build.success, build.logs.join('\n'));
const javascript = await build.outputs[0].text();
const chromePath = resolveChrome(process.env.CHROME_PATH);
const profile = resolveProfileDir(null, 'history-scroll');
const server = Bun.serve({
    hostname: '127.0.0.1',
    port: 0,
    fetch(request) {
        return new URL(request.url).pathname === '/app.js'
            ? new Response(javascript, { headers: { 'Content-Type': 'application/javascript' } })
            : new Response('<!doctype html><div id="root"></div><script type="module" src="/app.js"></script>', {
                headers: { 'Content-Type': 'text/html' },
            });
    },
});
const port = await reservePort();
const chrome = launchChrome({ chrome: chromePath, port, profileDir: profile.dir, headless: true });
let client;
try {
    const target = await createPageTarget(port);
    client = new CdpClient(target.webSocketDebuggerUrl);
    await client.connect();
    await client.send('Runtime.enable');
    await client.send('Page.enable');
    const exceptions = [];
    client.on('Runtime.exceptionThrown', event => exceptions.push(event.exceptionDetails));
    for (const direction of ['ltr', 'rtl']) for (const [period, step, base] of heightProfiles) for (const moveWhileLoading of [false, true]) {
        const loaded = client.once('Page.loadEventFired');
        await client.send('Page.navigate', {
            url: `http://127.0.0.1:${server.port}/?moving=${moveWhileLoading}&period=${period}&step=${step}&base=${base}&dir=${direction}`,
        });
        await loaded;
        let ready = false;
        for (let attempt = 0; attempt < 100; attempt++) {
            ready = await evaluateValue(client, 'typeof window.runScenario === "function"');
            if (ready) break;
            await wait(100);
        }
        assert.ok(ready, 'The browser fixture mounted');
        const result = await evaluateValue(client, `window.runScenario(${moveWhileLoading})`);
        console.log(JSON.stringify({ direction, period, step, base, moveWhileLoading, ...result }));
        assert.deepEqual(exceptions, [], 'No browser runtime errors');
        assert.equal(result.initialTailMounted, true, 'The initial tail is reachable after measurement');
        assert.ok(Math.abs(result.initialEndGap) <= 1, 'Initial size estimates settle at the real end');
        assert.equal(result.initialViewportCovered, true, 'Short rows fill the viewport after measurement');
        if (base === 2400) {
            assert.ok(result.initialMountCount <= 3, 'Opening tall history mounts only the tail and nearby overscan');
        }
        assert.equal(result.dataCount, 81, 'The prepend and tail append actually committed');
        assert.equal(result.frameCount, 70);
        assert.ok(result.elapsed < 5_000, 'The renderer was producing frames without background throttling');
        assert.equal(result.missingFrames, 0, 'The visible row stayed mounted');
        assert.ok(result.maxShift <= 1, `History shifted the visible anchor by ${result.maxShift}px`);
        assert.ok(Math.abs(result.tailGrowthShift) <= 1, 'Content below the reader must not move them');
    }
} finally {
    try {
        await stopChrome(chrome, client);
    } finally {
        client?.socket.close();
        server.stop(true);
        await profile.remove();
    }
}
