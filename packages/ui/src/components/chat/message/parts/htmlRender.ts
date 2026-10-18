import { z } from 'zod';

import type { ChatQuoteAnchor } from '@/lib/chatQuoteAnchor';
import type { JsonValue } from '@/lib/opencode/model';
import { getReadableThemeColors } from '@/lib/theme/readableColors';
import type { Theme } from '@/types/theme';

/**
 * Pages an agent shows with the `openchamber_render` tool. The page is the
 * call's own `html` input; the chat draws it in a sandboxed frame without
 * `allow-same-origin`, so its scripts run in an opaque origin with no access to
 * OpenChamber's storage, credentials or DOM.
 *
 * A small bootstrap goes into the page's head ahead of its own styles. It sets
 * the app theme as CSS variables (and swaps them when the host posts a new
 * theme), reports the page's content height so the frame fits it, and hands
 * http(s) link clicks to the host, which opens them in the browser; a page
 * never navigates the frame away from itself.
 *
 * A page that waits for the user's answer (`awaitAnswer`) sends it with
 * `openchamber.submit(data, summary)`, an MCP Apps `ui/message` request the
 * host answers with a JSON-RPC result or error.
 */

export const HTML_RENDER_MIN_HEIGHT = 48;
// A page takes its full height in the chat and never scrolls inside its
// frame: a scrolling frame keeps the wheel or a swipe that starts on it, so
// the chat stopped scrolling under the pointer until the page reached its own
// end, and a page's button could sit out of view below the frame's edge. This
// ceiling only stops a page whose height grows with its frame from growing
// without end. The agent-facing description in
// packages/web/server/lib/agent-tool/runtime.js states the behavior.
const HTML_RENDER_MAX_HEIGHT = 20_000;

const MESSAGE_PREFIX = 'openchamber:html-render:';
const THEME_MESSAGE = `${MESSAGE_PREFIX}theme`;
const SIZE_MESSAGE = `${MESSAGE_PREFIX}size`;
const OPEN_LINK_MESSAGE = `${MESSAGE_PREFIX}open-link`;
const KEY_MESSAGE = `${MESSAGE_PREFIX}key`;
const ANSWER_STATE_MESSAGE = `${MESSAGE_PREFIX}answer-state`;
const SELECTION_MESSAGE = `${MESSAGE_PREFIX}selection`;
// Escape pressed in the page while it has a reported selection: the host
// closes the selection menu, as Escape does for a selection in the chat.
const SELECTION_ESCAPE_MESSAGE = `${MESSAGE_PREFIX}selection-escape`;
const MARK_CLICK_MESSAGE = `${MESSAGE_PREFIX}mark-click`;
// From the host: drop the page's own selection, paint these quote marks, and
// scroll to and flash one quote.
const CLEAR_SELECTION_MESSAGE = `${MESSAGE_PREFIX}clear-selection`;
const MARKS_MESSAGE = `${MESSAGE_PREFIX}marks`;
const REVEAL_MESSAGE = `${MESSAGE_PREFIX}reveal`;
// The longest selection a page reports; the bootstrap cuts at this length.
const HTML_RENDER_SELECTION_MAX_LENGTH = 20_000;
// How long a revealed quote flashes; the same as the chat's own flash.
const HTML_RENDER_FLASH_DURATION_MS = 1600;

export interface HtmlRenderTheme {
    appearance: 'light' | 'dark';
    variables: Record<string, string>;
}

interface HtmlRenderMetrics {
    /** The app's rem in px: the interface size setting scales it, and the controls with it. */
    rem: number;
    sans: string;
    mono: string;
}

/**
 * Chart series after the accent, the same in every theme. A theme's syntax
 * colors were used here first, but they are picked for reading code: in the
 * default theme they made near-twins (the accent orange beside the string
 * salmon, two teals, two greens), and the first two series of almost every
 * chart blurred together. These hues stay apart from each other and from any
 * accent; the light set is darker so it holds on a light background.
 */
const CHART_SERIES = {
    dark: ['#60a5fa', '#2dd4bf', '#a78bfa', '#f472b6', '#a3e635'],
    light: ['#2563eb', '#0d9488', '#7c3aed', '#db2777', '#65a30d'],
} as const;

/**
 * The variables a page styles against. The names follow the shadcn
 * convention models already write. The agent-facing description in
 * packages/web/server/lib/agent-tool/runtime.js lists them; change both together.
 */
export function htmlRenderTheme(theme: Theme, metrics: HtmlRenderMetrics): HtmlRenderTheme {
    const { surface, interactive, primary, status, syntax } = theme.colors;
    const dark = theme.metadata.variant === 'dark';
    // The app's main button (components/ui/button.tsx, TINT_PRIMARY): a pale
    // accent fill and border with readable accent text. A dark theme mixes the
    // accent into transparent so the tone sits on the surface behind it.
    const tint = (amount: number, base: string) => `color-mix(in srgb,${primary.base} ${amount}%,${base})`;
    const readable = getReadableThemeColors(theme);
    return {
        appearance: theme.metadata.variant,
        variables: {
            '--background': surface.background,
            '--foreground': surface.foreground,
            '--muted': surface.muted,
            '--muted-foreground': surface.mutedForeground,
            '--card': surface.elevated,
            '--card-foreground': surface.elevatedForeground,
            '--border': interactive.border,
            '--primary': primary.base,
            '--primary-foreground': primary.foreground ?? surface.background,
            '--destructive': status.error,
            '--warning': status.warning,
            '--success': status.success,
            '--info': status.info,
            '--code-background': syntax.base.background,
            '--code-foreground': syntax.base.foreground,
            // The first series is the theme's accent, so the main one reads
            // as the app's own; the rest are a fixed set (CHART_SERIES).
            '--chart-1': primary.base,
            ...Object.fromEntries(CHART_SERIES[theme.metadata.variant].map((color, index) => [`--chart-${index + 2}`, color])),
            '--oc-button-text': readable.tinted.primary,
            '--oc-button-bg': dark ? tint(16, 'transparent') : tint(10, surface.background),
            '--oc-button-hover': dark ? tint(22, 'transparent') : tint(16, surface.background),
            '--oc-button-active': dark ? tint(30, 'transparent') : tint(22, surface.background),
            '--oc-button-border': tint(dark ? 20 : 12, 'transparent'),
            // The light the app's filled buttons take (`.oc-raised-button`
            // in styles/design-system.css): a lit top line and a short
            // shadow, plus a faint sheen in dark themes.
            '--oc-button-shadow': dark
                ? `inset 0 1px 0 0 color-mix(in srgb,${surface.foreground} 6%,transparent), 0 1px 1px 0 rgb(0 0 0 / 0.31), 0 2px 5px -2px rgb(0 0 0 / 0.4)`
                : 'inset 0 1px 0 0 rgb(255 255 255 / 0.45), 0 1px 2px 0 rgb(0 0 0 / 0.08)',
            '--oc-button-sheen': dark ? `linear-gradient(180deg,color-mix(in srgb,${surface.foreground} 1%,transparent),transparent 50%)` : 'none',
            // What the app marks chosen with (a pressed chip): the selection
            // fill and its readable text.
            '--oc-selection': interactive.selection,
            '--oc-selection-foreground': readable.selectionForeground,
            '--oc-rem': `${metrics.rem}px`,
            // The app's --radius (0.5625rem), at the app's scale.
            '--radius': `${metrics.rem * 0.5625}px`,
            '--font-sans': metrics.sans,
            '--font-mono': metrics.mono,
            // Not listed for agents: the fill of a quote mark the chat paints
            // in the page (see BASE_CSS), the same as the chat's own marks.
            '--oc-quote-mark': interactive.selection,
        },
    };
}

// Ready classes for a page that asks the user to choose, in the app's look:
// a grid of options, a selectable option card (the chosen one carries
// aria-pressed="true") and the main button, drawn like the app's own main
// button (the --oc-button-* and --oc-selection* variables from htmlRenderTheme). Agents style these themselves
// otherwise, which costs output on every such page and looks different each
// time. The agent-facing description in
// packages/web/server/lib/agent-tool/runtime.js names them; change both together.
const OPTION_CSS = '.oc-options{display:grid;gap:12px;grid-template-columns:repeat(auto-fit,minmax(min(200px,100%),1fr))}'
    // Option cards are the app's flat tiles (`.oc-tile`, `.oc-tile-hover` in
    // styles/design-system.css): no border, set off by tone, a tone step more
    // on hover, and the selection fill once chosen, like a pressed chip.
    + '.oc-option{display:flex;flex-direction:column;justify-content:flex-start;align-items:flex-start;gap:4px;box-sizing:border-box;width:100%;padding:16px;border:0;border-radius:var(--radius);background-color:color-mix(in srgb,var(--foreground) 4%,transparent);color:var(--foreground);font:inherit;text-align:start;cursor:pointer;transition:background-color .15s,color .15s}'
    + '.oc-option:hover,.oc-option:focus-visible{background-color:color-mix(in srgb,var(--foreground) 7%,transparent)}'
    + '.oc-option[aria-pressed=true]{background-color:var(--oc-selection);color:var(--oc-selection-foreground)}'
    // Sized in the app's rem like its Button (h-8 px-3 rounded-md, the UI
    // label size), so it follows the interface size setting.
    + '.oc-button{display:inline-flex;align-items:center;justify-content:center;box-sizing:border-box;height:calc(var(--oc-rem) * 2);padding:0 calc(var(--oc-rem) * .75);border:1px solid var(--oc-button-border);border-radius:calc(var(--oc-rem) * .4375);background-color:var(--oc-button-bg);background-image:var(--oc-button-sheen);box-shadow:var(--oc-button-shadow);color:var(--oc-button-text);font:inherit;font-size:calc(var(--oc-rem) * .84375);line-height:1.45;font-weight:500;white-space:nowrap;cursor:pointer;transition:background-color .15s}'
    + '.oc-button:hover,.oc-button:focus-visible{background-color:var(--oc-button-hover)}.oc-button:active{background-color:var(--oc-button-active);background-image:none;box-shadow:none}'
    + '.oc-button:disabled{opacity:.5;cursor:default;pointer-events:none}'
    // Focus draws no ring in the app: a focused control takes its hover fill.
    + '.oc-option:focus-visible,.oc-button:focus-visible{outline:none}';

// The page sits on the chat's own background, and a scrollbar inside the
// reply would read as a box within the thread, so it stays hidden. Media
// shrinks to the column. What is still wider (a wide table) scrolls sideways,
// without the elastic bounce at its edges; the page never scrolls vertically
// while it fits its frame (see the bootstrap), so a vertical swipe still
// scrolls the chat. Only the horizontal bounce is turned off: turning off the
// vertical one would also stop a vertical swipe from reaching the chat.
const BASE_CSS = 'html{background:transparent;color:var(--foreground);font-family:var(--font-sans);font-size:14px;line-height:1.5;-webkit-font-smoothing:antialiased;-webkit-text-size-adjust:100%;scrollbar-width:none;overflow-x:auto;overscroll-behavior-x:none}'
    + 'html::-webkit-scrollbar{display:none}body{margin:0}a{color:var(--primary)}code,kbd,pre,samp{font-family:var(--font-mono)}img,svg,video,canvas{max-width:100%}'
    + '::highlight(oc-chat-quote),::highlight(oc-chat-quote-focus),::highlight(oc-chat-quote-flash){background-color:var(--oc-quote-mark,rgba(127,127,127,.35))}'
    + OPTION_CSS;

const CSS_VARIABLE_NAME = /^--[a-z0-9-]+$/;
const sanitizeCssValue = (value: string): string => value.replace(/[;{}<>]/g, '');

function themeCss(theme: HtmlRenderTheme): string {
    const declarations = Object.entries(theme.variables)
        .filter(([name]) => CSS_VARIABLE_NAME.test(name))
        .map(([name, value]) => `${name}:${sanitizeCssValue(value)};`)
        .join('');
    return `:root{color-scheme:${theme.appearance};${declarations}}${BASE_CSS}`;
}

// Runs in the page's head, before its own styles and scripts. It rewrites its
// own <style> element rather than setting inline properties, so the page's
// own :root rules still win. The height it reports is the document's content
// height: the root's box when the page is shorter than the frame, its scroll
// height when the page is taller. Either way it reaches down to where the
// body's content ends, overflow included (unless the body clips it), and to
// the lowest absolutely positioned element that is not inside a box that
// scrolls or clips (from the set the tooltip check below keeps; a fixed one
// sits in the frame's own viewport and adds no height). The root's scroll
// height counts content past the root's box only while the frame is shorter
// than where it ends, so a tooltip parked below the page's last block, or a
// block taller than the box it sits in, made the frame grow to it, then
// shrink back to the root's box, on every frame for as long as the page was
// shown. The body's own scroll height does not depend on the frame's. It is
// measured before the page's own `zoom`, so it is scaled by the body's drawn
// height over its layout height to match the box it is added to.
//
// A page that fits its frame cannot scroll at all. A document that can scroll
// keeps a touch that starts on it: on a phone, a swipe over a page that fit
// its frame bounced inside it instead of scrolling the chat. Only a page taller
// than its frame (past the height cap, or in the full-screen view) scrolls.
//
// A focused frame keeps every key from the app, so a Cmd/Ctrl chord the page
// left alone is passed to the host, which replays it through the app's own
// shortcut handling. The page keeps what it handled itself, typing in its own
// fields, and the clipboard, select-all and undo chords. The deciding listener
// is added while the key dispatches, so it runs after every listener the page
// has and sees whether one of them took the key.
//
// The app cannot read a selection inside the frame, so the bootstrap reports
// it: the selected text and its box in the frame's viewport once it settles (on
// mouse up, or after a pause for keyboard and touch selections; not while the
// mouse is down), and an empty text when the selection is gone. The host shows
// the chat's selection menu from it. The report also carries the selection as
// a quote anchor (text, 32 characters either side, start offset) measured in
// the text stream of the page's own body, the same algorithm as
// lib/chatQuoteAnchor.ts, so the quote can be found again. When the app takes
// the selection over, the host posts a clear message: the page drops its
// selection and, having reset its last report first, does not echo that back.
// Escape the page left alone is passed on only while a reported selection
// stands, so the host closes the menu as Escape does in the chat; with no
// selection, Escape stays the page's own.
//
// Quotes the user commented on are painted back into the page. The host posts
// the marks (id, anchor, whether drawn stronger) whenever they change and when
// the frame loads; the page re-finds each by its text, as the chat does for
// its own messages, and paints them with the CSS Custom Highlight API (colored
// by --oc-quote-mark; see BASE_CSS). It repaints when the DOM changes. A click
// reports the mark under it with its box, or an empty id for a click on no
// mark, so the host can open or close the mark's popover. A reveal message
// scrolls one anchor into view and flashes it. In the chat the frame is as
// tall as the page; the browser carries that scroll on to the chat around the
// frame, so the quote comes into view there too.
//
// A tooltip or popup the page positions itself can stick out past the frame,
// where the frame cuts it off (on a phone the column is about 360px). Every
// absolutely or fixed positioned element that is partly inside and partly out
// is moved back in with the `translate` property, which the page's own
// `transform` composes with. An element wholly outside (a hidden off-screen
// panel), wider than the frame, or inside a box that scrolls or clips (where
// running past the edge is the point) is left alone. The positioned set is
// rebuilt when the DOM changes shape and checked on every change, hover, tap
// and resize; a CSS :hover tooltip changes nothing in the DOM, so hover counts.
const BOOTSTRAP_SCRIPT = `(function(){var s=document.getElementById("oc-html-render-theme");if(!s||window.parent===window)return;var B=${JSON.stringify(BASE_CSS)};`
    + `window.addEventListener("message",function(e){var d=e.data;if(e.source!==window.parent||!d||d.type!==${JSON.stringify(THEME_MESSAGE)}||!d.variables||typeof d.variables!=="object")return;var c=":root{color-scheme:"+(d.appearance==="light"?"light":"dark")+";";for(var k in d.variables){if(/^--[a-z0-9-]+$/.test(k))c+=k+":"+String(d.variables[k]).replace(/[;{}<>]/g,"")+";";}s.textContent=c+"}"+B;});`
    + `document.addEventListener("click",function(e){var t=e.target,a=t&&t.closest?t.closest("a[href]"):null,u;if(!a)return;try{u=new URL(a.getAttribute("href"),document.baseURI);}catch(x){return;}if(!/^https?:$/.test(u.protocol))return;e.preventDefault();window.parent.postMessage({type:${JSON.stringify(OPEN_LINK_MESSAGE)},url:u.href},"*");},true);`
    + `var h,z=function(){var r=document.documentElement,d=document.body,v=r.scrollHeight>r.clientHeight?r.scrollHeight:r.getBoundingClientRect().height,i,b;`
    + `if(d&&!/^(?:hidden|clip|scroll|auto)$/.test(getComputedStyle(d).overflowY)){b=d.getBoundingClientRect();b=b.top+window.scrollY+(d.clientTop+d.scrollHeight)*(d.offsetHeight?b.height/d.offsetHeight:1);if(b>v)v=b;}if(N){N=0;W();}`
    + `for(i=0;i<P.length;i++){b=P[i].getBoundingClientRect();if(getComputedStyle(P[i]).position==="absolute"&&(b.width||b.height)&&b.bottom+window.scrollY>v)v=b.bottom+window.scrollY;}v=Math.ceil(v);r.style.overflowY=v>window.innerHeight+1?"auto":"hidden";if(v===h)return;h=v;window.parent.postMessage({type:${JSON.stringify(SIZE_MESSAGE)},height:v},"*");};window.addEventListener("resize",z);`
    + `if(window.ResizeObserver){var o=new ResizeObserver(z);o.observe(document.documentElement);document.addEventListener("DOMContentLoaded",function(){if(document.body)o.observe(document.body);});}`
    + `document.addEventListener("DOMContentLoaded",z);window.addEventListener("load",z);`
    + `window.addEventListener("keydown",function(e){var t=e.composedPath()[0],f;if(!e.isTrusted||e.isComposing||!(e.metaKey||e.ctrlKey)||/^[acvxyz]$/i.test(e.key)&&!e.altKey||t&&(t.isContentEditable||/^(?:INPUT|TEXTAREA|SELECT)$/.test(t.tagName)))return;`
    + `f=function(v){window.removeEventListener("keydown",f);if(v!==e||e.defaultPrevented)return;window.parent.postMessage({type:${JSON.stringify(KEY_MESSAGE)},key:e.key,code:e.code,metaKey:e.metaKey,ctrlKey:e.ctrlKey,shiftKey:e.shiftKey,altKey:e.altKey,repeat:e.repeat},"*");};`
    + `window.addEventListener("keydown",f);setTimeout(function(){window.removeEventListener("keydown",f);});},true);`
    + `var SL="",ST=0,SD=0,SF=function(f){clearTimeout(ST);ST=0;var s=window.getSelection(),t=s?s.toString().trim().slice(0,${HTML_RENDER_SELECTION_MAX_LENGTH}):"",r={top:0,left:0,width:0,height:0},b,a=null;`
    + `if(t&&s.rangeCount){b=s.getRangeAt(0).getBoundingClientRect();r={top:b.top,left:b.left,width:b.width,height:b.height};a=AK(s.getRangeAt(0));}else t="";`
    + `if(t===SL&&!(f&&t))return;SL=t;window.parent.postMessage({type:${JSON.stringify(SELECTION_MESSAGE)},text:t,rect:r,anchor:a},"*");};`
    + `document.addEventListener("selectionchange",function(){if(SD)return;clearTimeout(ST);ST=setTimeout(SF,200);});`
    + `document.addEventListener("mousedown",function(){SD=1;},true);document.addEventListener("mouseup",function(){SD=0;clearTimeout(ST);ST=setTimeout(function(){SF(1);},10);},true);`
    + `window.addEventListener("keydown",function(e){var f;if(e.key!=="Escape"||!SL||!e.isTrusted||e.isComposing)return;f=function(v){window.removeEventListener("keydown",f);if(v!==e||e.defaultPrevented)return;window.parent.postMessage({type:${JSON.stringify(SELECTION_ESCAPE_MESSAGE)}},"*");};`
    + `window.addEventListener("keydown",f);setTimeout(function(){window.removeEventListener("keydown",f);});},true);`
    + `var AM=[],AR=[],AF=0,AY=0,AT=function(){return document.body?document.body.textContent||"":"";},`
    + `AO=function(n,o){var r=document.createRange();r.setStart(document.body,0);r.setEnd(n,o);return r.toString().length;},`
    + `AK=function(g){var b=document.body,t,x,a,e;if(!b||!b.contains(g.startContainer)||!b.contains(g.endContainer))return null;t=AT();a=AO(g.startContainer,g.startOffset);e=AO(g.endContainer,g.endOffset);x=t.slice(a,e);`
    + `if(!x.trim()||x.length>${HTML_RENDER_SELECTION_MAX_LENGTH})return null;return{text:x,prefix:t.slice(Math.max(0,a-32),a),suffix:t.slice(e,e+32),start:a};},`
    + `AL=function(t,a){var o=[],i=a.text?t.indexOf(a.text):-1,c;for(;i!==-1&&o.length<200;i=t.indexOf(a.text,i+1))o.push(i);if(o.length<2)return o.length?o[0]:-1;if(!a.prefix&&!a.suffix)return -1;`
    + `c=o.filter(function(i){return i>=a.prefix.length&&t.startsWith(a.prefix,i-a.prefix.length)&&t.startsWith(a.suffix,i+a.text.length);});return c.length?c.reduce(function(m,i){return Math.abs(i-a.start)<Math.abs(m-a.start)?i:m;}):-1;},`
    + `AQ=function(a){var s=AL(AT(),a),e=s+a.text.length,g=document.createRange(),w,n,o=0,k=0,l;if(s<0)return null;w=document.createTreeWalker(document.body,4);`
    + `while(n=w.nextNode()){l=n.data.length;if(!k&&s<o+l){g.setStart(n,s-o);k=1;}if(k&&e<=o+l){g.setEnd(n,e-o);return g;}o+=l;}return null;},`
    + `AV=function(m){var a=m&&m.anchor;return!!a&&typeof m.id==="string"&&typeof a.text==="string"&&typeof a.prefix==="string"&&typeof a.suffix==="string"&&typeof a.start==="number";},`
    + `AH=function(n,r){if(!window.Highlight||!window.CSS||!CSS.highlights)return;if(r.length)CSS.highlights.set(n,new Highlight(...r));else CSS.highlights.delete(n);},`
    + `AP=function(){var m=[],f=[],i,r;AR=[];for(i=0;i<AM.length;i++){r=AQ(AM[i].anchor);if(!r)continue;AR.push({id:AM[i].id,range:r});m.push(r);if(AM[i].focused)f.push(r);}AH("oc-chat-quote",m);AH("oc-chat-quote-focus",f);};`
    + `window.addEventListener("message",function(e){var d=e.data,g;if(e.source!==window.parent||!d)return;`
    + `if(d.type===${JSON.stringify(CLEAR_SELECTION_MESSAGE)}){SL="";clearTimeout(ST);g=window.getSelection();if(g)g.removeAllRanges();}`
    + `else if(d.type===${JSON.stringify(MARKS_MESSAGE)}&&Array.isArray(d.marks)){AM=d.marks.filter(AV);AP();}`
    + `else if(d.type===${JSON.stringify(REVEAL_MESSAGE)}&&AV({id:"",anchor:d.anchor})){g=AQ(d.anchor);if(!g)return;(g.startContainer.parentElement||document.body).scrollIntoView({block:"center",inline:"nearest"});`
    + `AH("oc-chat-quote-flash",[g]);clearTimeout(AF);AF=setTimeout(function(){AH("oc-chat-quote-flash",[]);},${HTML_RENDER_FLASH_DURATION_MS});}});`
    + `new MutationObserver(function(){if(AM.length){clearTimeout(AY);AY=setTimeout(AP,100);}}).observe(document.documentElement,{subtree:true,childList:true,characterData:true});`
    + `document.addEventListener("click",function(e){var g=window.getSelection(),x=e.clientX,y=e.clientY,i,j,c,b=null,k="";if(!AR.length||g&&!g.isCollapsed)return;`
    + `for(i=0;i<AR.length&&!b;i++){c=AR[i].range.getClientRects();for(j=0;j<c.length;j++){if(x>=c[j].left&&x<=c[j].right&&y>=c[j].top&&y<=c[j].bottom){k=AR[i].id;b=AR[i].range.getBoundingClientRect();break;}}}`
    + `window.parent.postMessage({type:${JSON.stringify(MARK_CLICK_MESSAGE)},id:k,rect:b?{top:b.top,left:b.left,width:b.width,height:b.height}:{top:y,left:x,width:0,height:0}},"*");});`
    + `var P=[],Q=0,N=1,C=function(e){for(var a=e.parentElement;a&&a!==document.body;a=a.parentElement){var o=getComputedStyle(a);if(o.overflowX!=="visible"||o.overflowY!=="visible")return 1;}return 0;},`
    + `W=function(){P=[];var a=document.body?document.body.getElementsByTagName("*"):[];for(var i=0;i<a.length&&i<5000;i++){var p=getComputedStyle(a[i]).position;if((p==="absolute"||p==="fixed")&&!C(a[i]))P.push(a[i]);}},`
    + `F=function(){Q=0;if(N){N=0;W();}var w=document.documentElement.clientWidth;for(var i=0;i<P.length;i++){var e=P[i],x=e.__ocx||0,y=e.__ocy||0,r=e.getBoundingClientRect(),l=r.left-x,g=r.right-x,t=r.top-y,b=r.bottom-y,nx=0,ny=0;if(!r.width&&!r.height)continue;`
    + `if(g>0&&l<w&&r.width<=w){if(g>w)nx=w-g;if(l+nx<0)nx=-l;}if(t<0&&b>0)ny=-t;if(nx!==x||ny!==y){e.__ocx=nx;e.__ocy=ny;e.style.translate=nx||ny?nx+"px "+ny+"px":"";}}},`
    + `U=function(r){if(r)N=1;if(!Q)Q=requestAnimationFrame(F);};`
    + `new MutationObserver(function(m){for(var i=0;i<m.length;i++)if(m[i].type==="childList"||m[i].attributeName==="class")N=1;U(0);}).observe(document.documentElement,{subtree:true,childList:true,attributes:true,attributeFilter:["style","class"]});`
    + `["mouseover","click","touchstart","scroll","resize"].forEach(function(n){window.addEventListener(n,function(){U(0);},{capture:true,passive:true});});document.addEventListener("DOMContentLoaded",function(){U(1);});})();`;

/**
 * What the page knows about its answer: whether it may still send one, and
 * what it sent. The page reads the second as `openchamber.answer`, so a page
 * loaded again after the answer can show the choice.
 */
export interface HtmlRenderAnswerState {
    waiting: boolean;
    answer: JsonValue | null;
}

// `openchamber.submit` checks what it can in the page (the host checks the
// gesture and the waiting state again, since a page can skip this script) and
// sends `ui/message` with the summary as the text and the data in `_meta`.
// The host's reply settles the promise; a state message from the host ends
// the wait when it ended elsewhere (another device answered, or the user
// wrote in the chat).
const ANSWER_SCRIPT = `(function(){if(window.parent===window)return;var S=JSON.parse(document.getElementById("oc-html-render-answer").textContent),I=0,R={};`
    + `var o=window.openchamber={answer:S.answer===null?undefined:S.answer,submit:function(data,summary){return new Promise(function(res,rej){`
    + `if(!S.waiting)return rej(new Error("This page is not waiting for an answer"));`
    + `if(navigator.userActivation&&!navigator.userActivation.isActive)return rej(new Error("openchamber.submit works only from a click"));`
    + `if(typeof summary!=="string"||!summary.trim())return rej(new Error("summary is required: the choice in a few words"));`
    + `var id=++I,v=data===undefined?null:data;R[id]={res:res,rej:rej,data:v};`
    + `try{window.parent.postMessage({jsonrpc:"2.0",id:id,method:"ui/message",params:{role:"user",content:[{type:"text",text:summary}],_meta:{"openchamber/data":v}}},"*");}catch(x){delete R[id];rej(x);}});}};`
    + `window.addEventListener("message",function(e){var d=e.data,r;if(e.source!==window.parent||!d)return;`
    + `if(d.type===${JSON.stringify(ANSWER_STATE_MESSAGE)}){S={waiting:d.waiting===true,answer:d.answer===undefined?null:d.answer};o.answer=S.answer===null?undefined:S.answer;return;}`
    + `if(d.jsonrpc!=="2.0"||!R[d.id])return;r=R[d.id];delete R[d.id];`
    + `if(d.error)r.rej(new Error(String(d.error.message||"The answer was not sent")));else{S={waiting:false,answer:r.data};o.answer=r.data;r.res();}});})();`;

// JSON inside a script element: "<" is escaped so no "</script>" in the data
// can close it.
const scriptJson = (value: HtmlRenderAnswerState): string => JSON.stringify(value).replace(/</g, '\\u003c');

// Comments and raw-text elements are blanked to the same length before the
// head is located, so a "<head>" written inside a script or comment is never
// where the bootstrap lands.
const blankNonMarkup = (html: string): string => html.replace(
    /<!--[\s\S]*?(?:-->|$)|<(script|style|textarea|title|template|noscript)\b[\s\S]*?(?:<\/\1\s*>|$)/gi,
    (match) => ' '.repeat(match.length),
);

/**
 * The document the frame loads: the agent's page with the bootstrap first in
 * its head. `contained` is for a frame with nothing around it to scroll (the
 * full-screen view): the page's own scrolling stops at its edges without the
 * elastic bounce. `answerState` is where the page stands on its answer when
 * it loads; later changes reach it as messages.
 */
export function buildHtmlRenderDocument(
    html: string,
    theme: HtmlRenderTheme,
    { contained = false, answerState = { waiting: false, answer: null } }: { contained?: boolean; answerState?: HtmlRenderAnswerState } = {},
): string {
    const scan = blankNonMarkup(html);
    const bootstrap = [
        /<meta\s[^>]*charset/i.test(scan.slice(0, 4096)) ? '' : '<meta charset="utf-8">',
        /<meta\s[^>]*name\s*=\s*["']?viewport/i.test(scan) ? '' : '<meta name="viewport" content="width=device-width, initial-scale=1">',
        `<style id="oc-html-render-theme">${themeCss(theme)}</style>`,
        // Its own element: a theme switch rewrites the theme style whole.
        contained ? '<style>html{overscroll-behavior:none}</style>' : '',
        `<script>${BOOTSTRAP_SCRIPT}</script>`,
        `<script type="application/json" id="oc-html-render-answer">${scriptJson(answerState)}</script>`,
        `<script>${ANSWER_SCRIPT}</script>`,
    ].join('');
    const headOpen = /<head(?:\s[^>]*)?>/i.exec(scan);
    if (headOpen) {
        const at = headOpen.index + headOpen[0].length;
        return html.slice(0, at) + bootstrap + html.slice(at);
    }
    const htmlOpen = /<html(?:\s[^>]*)?>/i.exec(scan);
    if (htmlOpen) {
        const at = htmlOpen.index + htmlOpen[0].length;
        return `${html.slice(0, at)}<head>${bootstrap}</head>${html.slice(at)}`;
    }
    const doctype = /^\s*<!doctype[^>]*>/i.exec(scan);
    if (doctype) {
        const at = doctype[0].length;
        return `${html.slice(0, at)}<head>${bootstrap}</head>${html.slice(at)}`;
    }
    return `<!doctype html><head>${bootstrap}</head>${html}`;
}

export const htmlRenderThemeMessage = (theme: HtmlRenderTheme) => ({ type: THEME_MESSAGE, ...theme });

export const htmlRenderAnswerStateMessage = (state: HtmlRenderAnswerState) => ({ type: ANSWER_STATE_MESSAGE, ...state });

type JsonRpcId = number | string;

/** The host's reply to a page's `ui/message`: an empty result, or an error the page's promise rejects with. */
export const htmlRenderSubmitReply = (id: JsonRpcId, error?: string) => (error === undefined
    ? { jsonrpc: '2.0', id, result: {} }
    : { jsonrpc: '2.0', id, error: { code: -32000, message: error } });

export const htmlRenderClearSelectionMessage = () => ({ type: CLEAR_SELECTION_MESSAGE });

// An anchor goes to the page as its text and context only: which page it is
// means nothing inside it.
const pageAnchor = ({ text, prefix, suffix, start }: ChatQuoteAnchor) => ({ text, prefix, suffix, start });

export const htmlRenderMarksMessage = (marks: readonly HtmlRenderMark[]) => ({
    type: MARKS_MESSAGE,
    marks: marks.map(({ id, anchor, focused }) => ({ id, anchor: pageAnchor(anchor), focused })),
});

export const htmlRenderRevealMessage = (anchor: ChatQuoteAnchor) => ({ type: REVEAL_MESSAGE, anchor: pageAnchor(anchor) });

const keyChordSchema = z.object({
    key: z.string().min(1).max(32),
    code: z.string().max(32),
    metaKey: z.boolean(),
    ctrlKey: z.boolean(),
    shiftKey: z.boolean(),
    altKey: z.boolean(),
    repeat: z.boolean(),
});

/** A key a page passed on; the host replays it as a keydown of its own. */
export type HtmlRenderKeyChord = z.infer<typeof keyChordSchema>;

/** What a page sent with `openchamber.submit`. */
export interface HtmlRenderSubmission {
    summary: string;
    data: JsonValue;
}

const selectionRectSchema = z.object({
    top: z.number().finite(),
    left: z.number().finite(),
    width: z.number().finite().min(0),
    height: z.number().finite().min(0),
});

// The anchor a page measured in its own body; the host adds which page it is.
const pageAnchorSchema = z.object({
    text: z.string().min(1).max(HTML_RENDER_SELECTION_MAX_LENGTH),
    prefix: z.string().max(64),
    suffix: z.string().max(64),
    start: z.number().int().min(0),
});

type HtmlRenderFrameMessage =
    | { kind: 'size'; height: number }
    | { kind: 'open-link'; url: string }
    | { kind: 'key'; chord: HtmlRenderKeyChord }
    | { kind: 'submit'; id: JsonRpcId; submission: HtmlRenderSubmission }
    // An empty text means the selection is gone. No anchor: the selection
    // could not be anchored (it reaches outside the body, or is too long).
    | { kind: 'selection'; text: string; rect: z.infer<typeof selectionRectSchema>; anchor?: Omit<ChatQuoteAnchor, 'page'> }
    // An empty id is a click on no mark; the rect is then the click's point.
    | { kind: 'selection-escape' }
    | { kind: 'mark-click'; id: string; rect: z.infer<typeof selectionRectSchema> };

/** A quote mark the host asks a page to paint. */
export interface HtmlRenderMark {
    id: string;
    anchor: ChatQuoteAnchor;
    /** Drawn stronger: hovered in the composer chips, or its popover is open. */
    focused: boolean;
}

const frameMessageSchema = z.discriminatedUnion('type', [
    z.object({ type: z.literal(SIZE_MESSAGE), height: z.number().positive() }),
    z.object({ type: z.literal(OPEN_LINK_MESSAGE), url: z.string().regex(/^https?:\/\//i) }),
    keyChordSchema.extend({ type: z.literal(KEY_MESSAGE) }),
    z.object({
        type: z.literal(SELECTION_MESSAGE),
        text: z.string().max(HTML_RENDER_SELECTION_MAX_LENGTH),
        rect: selectionRectSchema,
        // A page the anchor of which cannot be read still gets its menu.
        anchor: pageAnchorSchema.nullish().catch(null),
    }),
    z.object({ type: z.literal(SELECTION_ESCAPE_MESSAGE) }),
    z.object({ type: z.literal(MARK_CLICK_MESSAGE), id: z.string().max(200), rect: selectionRectSchema }),
]);

// MCP Apps `ui/message`. Only text content is read; the summary is its text.
const uiMessageSchema = z.object({
    jsonrpc: z.literal('2.0'),
    id: z.union([z.number(), z.string().max(64)]),
    method: z.literal('ui/message'),
    params: z.object({
        role: z.literal('user'),
        content: z.array(z.object({ type: z.literal('text'), text: z.string() })).min(1),
        _meta: z.object({ 'openchamber/data': z.json().optional() }).optional(),
    }),
});

/** The message a page's bootstrap posted, or null for anything else. */
export function readHtmlRenderFrameMessage(event: Pick<MessageEvent, 'data'>): HtmlRenderFrameMessage | null {
    const submit = uiMessageSchema.safeParse(event.data);
    if (submit.success) {
        const { id, params } = submit.data;
        return {
            kind: 'submit',
            id,
            submission: {
                summary: params.content.map((block) => block.text).join('\n').trim(),
                data: params._meta?.['openchamber/data'] ?? null,
            },
        };
    }
    const parsed = frameMessageSchema.safeParse(event.data);
    if (!parsed.success) return null;
    const message = parsed.data;
    if (message.type === SIZE_MESSAGE) return { kind: 'size', height: message.height };
    if (message.type === OPEN_LINK_MESSAGE) return { kind: 'open-link', url: message.url };
    if (message.type === SELECTION_MESSAGE) {
        const text = message.text.trim();
        const selection: Extract<HtmlRenderFrameMessage, { kind: 'selection' }> = { kind: 'selection', text, rect: message.rect };
        // Only a selection with text carries an anchor.
        if (text && message.anchor) selection.anchor = message.anchor;
        return selection;
    }
    if (message.type === SELECTION_ESCAPE_MESSAGE) return { kind: 'selection-escape' };
    if (message.type === MARK_CLICK_MESSAGE) return { kind: 'mark-click', id: message.id, rect: message.rect };
    // Only a chord with Cmd or Ctrl: the page never passes plain typing on,
    // and a page that tried to must not reach the app's single-key handling.
    if (!message.metaKey && !message.ctrlKey) return null;
    return { kind: 'key', chord: keyChordSchema.parse(message) };
}

export const clampHtmlRenderHeight = (height: number): number => (
    Math.min(HTML_RENDER_MAX_HEIGHT, Math.max(HTML_RENDER_MIN_HEIGHT, Math.ceil(height)))
);

/** A download name from the page title, without characters file systems reject. */
export function htmlRenderFileName(title: string): string {
    const name = title
        .replace(/[\\/:*?"<>|\p{Cc}]+/gu, ' ')
        .replace(/\s+/g, ' ')
        .trim()
        .slice(0, 120)
        .trim();
    return `${name || 'page'}.html`;
}
