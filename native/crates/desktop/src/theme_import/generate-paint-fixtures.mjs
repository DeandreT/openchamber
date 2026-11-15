// node generate-paint-fixtures.mjs <Chrome executable> <owned local staging dir>
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const [chrome, stage] = process.argv.slice(2);
if (!chrome || !stage) throw new Error('Chrome executable and owned local staging directory are required');
mkdirSync(stage, { recursive: true });
const module = resolve('native/crates/desktop/src/theme_import');
const paint = readFileSync(resolve(module, 'paint.rs'), 'utf8');
const names = [...paint.slice(paint.indexOf('fn named_color')).matchAll(/"([a-z]+)"/g)].map(match=>match[1]);
const values = [...names, '#abc', '#abcd', '#aabbcc88', 'transparent', 'currentColor',
  'rgb(255 204 102)', 'rgba(255, 204, 102, 0.5)', 'rgb(100% 80% 40% / 20%)',
  'rgba(100%, 80%, 40%, 0.7)', 'rgb(-20 400 12 / 150%)',
  'hsl(40 100% 70%)', 'hsla(40, 100%, 70%, 0.4)', 'hsl(-0.5turn 60% 40% / 75%)',
  'hsl(3.141592653589793rad 50% 60%)', 'hwb(40 20% 30% / 80%)', 'hwb(20 80% 70%)',
  'hsl(120 100 50)', 'hsla(120 100 50 / 0.4)', 'hsl(120 100% 50)', 'hsl(120 100 50%)',
  'hwb(120 30 50)', 'hwb(120 30% 50 / 50%)', 'hwb(120 30 50%)',
  'hwb(0 200% 100%)', 'hwb(0 200 100)', 'hwb(0 -20 50)', 'hsl(120 200 50)',
  'hsl(+1.2e2 +1E+2 5e1 / +.5)', 'hsl(120 100.0 50.0)',
  'rgb(+2.55e2 -0 .5e2 / 5E-1)', 'color-mix(in srgb, +2.0e1% red, blue)',
  'lab(70 20 -30)', 'lab(80% 30% -20% / 0.6)', 'lch(70 30 120)', 'lch(40% 20% 70deg / 30%)',
  'oklab(0.7 0.1 -0.1)', 'oklab(60% -20% 10% / 0.8)', 'oklch(0.85 0.12 80)', 'oklch(70% 30% 120 / 0.5)',
  ...['srgb','srgb-linear','display-p3','a98-rgb','prophoto-rgb','rec2020','xyz','xyz-d50','xyz-d65'].map(space=>`color(${space} 0.4 0.5 0.6 / 0.7)`),
  ...['srgb','srgb-linear','display-p3','a98-rgb','prophoto-rgb','rec2020','xyz','xyz-d50','xyz-d65','lab','lch','oklab','oklch','hsl','hwb'].map(space=>`color-mix(in ${space}, #ffcc66 40%, #4488aa)`),
  'color-mix(in srgb, rebeccapurple 20%, transparent)',
  'color-mix(in srgb, #ff000080 25%, #0000ff40 25%)',
  'color-mix(in srgb, #ff000080 80%, #0000ff40 80%)',
  'color-mix(in srgb, 70% red, 50% blue)',
  'color-mix(in srgb, 25% #ff000080, #0000ff40 25%)',
  'color-mix(in srgb, 20% rgb(255 0 0 / 0.5), blue)',
  'color-mix(in srgb, 90% color-mix(in oklab, #ffcc66 40%, transparent), #000000)',
  'color-mix(in oklab, #ffcc66 40%, transparent)',
  'color-mix(in srgb, color-mix(in oklab, #ffcc66 40%, transparent) 90%, #000000)',
  ...['shorter','longer','increasing','decreasing'].flatMap(direction=>['hsl','hwb','lch','oklch'].map(space=>`color-mix(in ${space} ${direction} hue, #ff9900 30%, #aa00ff)`)),
  'color-mix(in oklch, white, rebeccapurple)', 'color-mix(in hsl, gray, red)',
  'light-dark(#abcdef, #123456)',
  'color(display-p3 1 0 0)', 'oklch(0.7 0.4 40)',
];
const alphaSpaces = [
  ['rgb', ['120', '80', '40']], ['hsl', ['40', '60%', '50%']],
  ['hwb', ['40', '20%', '30%']], ['lab', ['60', '20', '-30']],
  ['lch', ['60', '30', '40']], ['oklab', ['0.6', '0.1', '-0.1']],
  ['oklch', ['0.6', '0.1', '40']],
  ...['srgb','srgb-linear','display-p3','a98-rgb','prophoto-rgb','rec2020','xyz','xyz-d50','xyz-d65'].map(space=>[space,['0.4','0.5','0.6']]),
];
function missingAlphaColor(space, channels) {
  return `${['rgb','hsl','hwb','lab','lch','oklab','oklch'].includes(space)?space+'(':`color(${space} `}${channels.join(' ')} / none)`;
}
for(const [space,channels] of alphaSpaces) {
 const color=missingAlphaColor(space,channels);
 values.push(color);
 for(const target of ['srgb','xyz-d50','hsl','hwb','lab','lch','oklab','oklch']) {
  values.push(`color-mix(in ${target}, ${color}, #6699cc)`);
  values.push(`color-mix(in ${target}, #6699cc, ${color})`);
  values.push(`color-mix(in ${target}, ${color}, ${color})`);
 }
}
values.push('rgba(120 80 40 / none)', 'hsla(40 60% 50% / none)',
 'color-mix(in srgb, rgb(200 100 50 / none), rgb(100 200 100 / 0.4))',
 'color-mix(in srgb, rgb(200 100 50 / none) 20%, rgb(100 200 100 / 0.4) 30%)',
 'color-mix(in srgb, color-mix(in srgb, rgb(200 100 50 / none), rgb(100 200 100 / none)), red)',
 'color-mix(in srgb, color-mix(in srgb, rgb(200 100 50 / none) 20%, rgb(100 200 100 / none) 30%), red)',
 'light-dark(rgb(255 0 0 / none), rgb(0 0 255 / none))');
const unsupported = ['rgb(from red r g b)', 'rgb(calc(20 + 30) 0 0)', 'oklch(0.7 none 40)', 'CanvasText', 'inherit'];
const invalid = ['hsl(120, 100, 50)', 'hsl(120, 100%, 50)', 'hsla(120, 100, 50%, 0.5)',
  'hwb(120, 30%, 50%)', 'color-mix(in srgb, 20% red 30%, blue)',
  'color-mix(in srgb, 101% red, blue)', 'color-mix(in srgb, -1% red, blue)',
  'hsl(120 100. 50)', 'color-mix(in srgb, 20.% red, blue)',
  'rgb(1.e2 0 0)', 'hsl(120 . 50)', 'rgb(1e+ 0 0)', 'rgb(++1 0 0)',
  'rgb(none, 0, 0)', 'rgba(0, 0, 0, none)', 'hsl(none, 50%, 50%)'];
const cases = values.map(value=>({value})).concat(unsupported.map(value=>({value,unsupported:true})), invalid.map(value=>({value,invalid:true})));
const page = `<!doctype html><meta charset="utf-8"><body style="color:#234567"><script>
const cases=${JSON.stringify(cases)};
const canvas=document.createElement('canvas');canvas.width=1;canvas.height=1;document.body.append(canvas);
const probe=document.createElement('span');document.body.append(probe);
const ctx=canvas.getContext('2d',{colorSpace:'srgb'});
const results=[];
for(const dark of [false,true]) {
 document.documentElement.style.colorScheme=dark?'dark':'light';
 for(const item of cases) {
  probe.style.color='';probe.style.color=item.value;
  const computed=getComputedStyle(probe).color;
  const samples=[];
  for(const background of ['#000000','#ffffff']) {
   ctx.clearRect(0,0,1,1);ctx.fillStyle=background;ctx.fillRect(0,0,1,1);
   ctx.fillStyle=computed;ctx.fillRect(0,0,1,1);
   samples.push([...ctx.getImageData(0,0,1,1).data].slice(0,3));
  }
  results.push({...item,dark,valid:CSS.supports('color',item.value),computed,samples});
 }
}
document.body.textContent=btoa(JSON.stringify({browser:navigator.userAgent,cases:results}));
</script>`;
const html = resolve(stage, 'paint-reference.html');
writeFileSync(html, page);
const browser = spawn(chrome, ['--headless=new','--disable-gpu','--disable-features=MacAppCodeSignClone','--disable-extensions','--disable-background-networking','--disable-component-update','--disable-sync','--no-first-run','--no-default-browser-check',`--user-data-dir=${resolve(stage,'chrome-profile')}`,'--remote-debugging-pipe','about:blank'], {stdio:['ignore','ignore','ignore','pipe','pipe'],windowsHide:true});
let sequence=0,buffer='';
const pending=new Map(),listeners=new Map();
browser.stdio[4].on('data',chunk=>{
  buffer+=chunk.toString();
  for(let end;(end=buffer.indexOf('\0'))!==-1;){
    const message=JSON.parse(buffer.slice(0,end));buffer=buffer.slice(end+1);
    if(message.id){const handler=pending.get(message.id);pending.delete(message.id);message.error?handler?.reject(new Error(JSON.stringify(message.error))):handler?.resolve(message.result);}
    else if(message.method){const key=`${message.sessionId??''}:${message.method}`;listeners.get(key)?.(message.params);listeners.delete(key);}
  }
});
function call(method,params={},sessionId){return new Promise((resolve,reject)=>{const id=++sequence;pending.set(id,{resolve,reject});const request={id,method,params};if(sessionId)request.sessionId=sessionId;browser.stdio[3].write(JSON.stringify(request)+'\0');});}
function event(method,sessionId){return new Promise(resolve=>listeners.set(`${sessionId}:${method}`,resolve));}
const timeout=setTimeout(()=>{for(const handler of pending.values())handler.reject(new Error('Owned Chrome fixture session timed out'));browser.kill();},30000);
let encoded;
try {
  const {targetId}=await call('Target.createTarget',{url:'about:blank'});
  const {sessionId}=await call('Target.attachToTarget',{targetId,flatten:true});
  await call('Page.enable',{},sessionId);
  const loaded=event('Page.loadEventFired',sessionId);
  await call('Page.navigate',{url:pathToFileURL(html).href},sessionId);
  await loaded;
  const result=await call('Runtime.evaluate',{expression:'document.body.textContent',returnByValue:true},sessionId);
  encoded=result.result.value;
  await call('Browser.close');
} finally {clearTimeout(timeout);if(!encoded)browser.kill();}
if(!encoded) throw new Error('Chrome did not return the fixture document');
const output = JSON.parse(Buffer.from(encoded,'base64').toString('utf8'));
output.referenceHtmlSha256=createHash('sha256').update(page).digest('hex');
writeFileSync(resolve(module,'paint-fixtures.json'),JSON.stringify(output,null,2)+'\n');
console.log(`Generated ${output.cases.length} painted color cases with ${output.browser}`);
