/** Run with Bun from the repository root. Optional dependency directory is an
 * existing node_modules/.bun directory; it permits a current-source fixture run
 * without installing or changing the dependencies of a different checkout. */
import { plugin } from 'bun';
import { createHash } from 'node:crypto';
import { readFile, readdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const root = process.cwd();
const deps = process.env.THEME_FIXTURE_DEPENDENCIES;
if (deps) {
  plugin({ name: 'theme-fixture-existing-dependencies', setup(builder) {
    builder.onResolve({ filter: /^(zod|strip-json-comments)$/ }, ({ path }) => ({ path: resolve(deps, path === 'zod' ? 'zod@4.3.6/node_modules/zod/index.js' : 'strip-json-comments@5.0.3/node_modules/strip-json-comments/index.js') }));
  } });
}
const themeRoot = resolve(root, 'packages/ui/src/lib/theme');
const { importVSCodeTheme } = await import(resolve(themeRoot, 'vscode/import.ts'));
const { requireTheme } = await import(resolve(themeRoot, 'definition.ts'));
const { getReadableThemeColors } = await import(resolve(themeRoot, 'readableColors.ts'));
const { agentColorPalette } = await import(resolve(root, 'packages/ui/src/lib/agentColorPalette.ts'));

function flatten(value: unknown, prefix = '', output: Record<string, string> = {}) {
  if (!value || typeof value !== 'object') return output;
  for (const [key, entry] of Object.entries(value)) {
    if (entry === undefined) continue;
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof entry === 'string') output[path] = entry;
    else flatten(entry, path, output);
  }
  return output;
}
function resolved(value: unknown) {
  const theme = requireTheme(value);
  return {
    metadata: theme.metadata,
    colors: { ...flatten(theme.colors), ...flatten(getReadableThemeColors(theme), 'readable') },
    agentPalette: agentColorPalette(theme).map(({ index, colorful }: {index: number; colorful: boolean}) => ({ index, colorful })),
  };
}

const rich = {
  name: 'Fixture Night', type: 'dark', author: 'Fixture author', description: 'Synthetic converter parity case',
  colors: {
    'editor.background': '#101820', 'editor.foreground': '#efefef',
    'sideBar.background': '#121416', 'editorWidget.background': '#fefefe', 'editorWidget.foreground': '#222222',
    'input.background': '#333333', 'input.foreground': '#dddddd',
    'list.activeSelectionBackground': '#223366', 'list.activeSelectionForeground': '#ccddff',
    'editor.selectionBackground': '#445566', 'inputOption.activeForeground': '#ff00ff',
    'button.background': '#ffcc44', 'button.foreground': '#000000',
    'gitDecoration.addedResourceForeground': '#33dd66', 'diffEditor.insertedLineBackground': '#33dd6620',
    'diffEditor.removedTextBackground': '#ff334420',
  },
  tokenColors: [
    { scope: 'comment', settings: { foreground: '#8899aa' } },
    { scope: ['keyword', 'storage'], settings: { foreground: '#aa66ff' } },
    { scope: 'keyword.control.import', settings: { foreground: '#cc88ff' } },
    { scope: 'source.js keyword', settings: { foreground: '#ff0000' } },
    { scope: 'string', settings: { foreground: '#99cc66' } },
    { scope: 'entity.name.function, support.function', settings: { foreground: '#44aaff' } },
    { scope: 'constant.numeric', settings: { foreground: '#ffaa55' } },
    { scope: 'variable', settings: { foreground: '#eeeeee' } },
    { scope: 'entity.name.type', settings: { foreground: '#66cccc' } },
    { scope: 'variable.other.property', settings: { foreground: '#ddaa66' } },
  ],
  semanticTokenColors: { function: '#55bbff', property: { foreground: '#eebb77' }, 'variable:typescript': '#ff0000' },
};
const imports: Array<{text: string; filename: string}> = [
  { text: JSON.stringify(rich), filename: 'fixture.json' },
  { text: '\uFEFF{ // comment\n "colors": { "editor.background": "#fff", }, }', filename: 'Paper.jsonc' },
  { text: JSON.stringify({ ...rich, semanticHighlighting: false, tokenColors: [...rich.tokenColors, {scope:'keyword',settings:{foreground:'#bb77ee'}}] }), filename: 'night.json' },
  { text: JSON.stringify({ ...rich, name: undefined, semanticTokenColors: {'*':'#abcdef',function:'#fedcba','variable:typescript':'#ff0000'} }), filename: 'dune-kaitain-color-theme.json' },
  { text: JSON.stringify({type:'hc-light',colors:{'editor.background':'#ffffff','editor.foreground':'#000000',contrastBorder:'#000000',focusBorder:'#111111'}}), filename:'hc-light.json' },
  { text: JSON.stringify({type:'hc-black',colors:{'editor.background':'#111111',contrastBorder:'#ffffff','input.border':'#aabbcc','chat.requestBorder':'#ff0000','textBlockQuote.border':'#00ff00'}}), filename:'hc-black.json' },
  { text: JSON.stringify({colors:{'editor.background':'#aab3','editor.foreground':'#ffd4','button.background':'#ace7','editorWarning.background':'#0000','input.border':'#0000'}}), filename:'alpha.json' },
  { text: JSON.stringify({colors:{'editor.background':'#101820','editor.foreground':'','symbolIcon.constantForeground':['#fff','#000']},tokenColors:[{settings:{foreground:'#aabbcc'}},{scope:[],settings:{foreground:'#ffffff'}},{scope:'comment',settings:{foreground:''}}]}), filename:'unset.json' },
  ...['#00151a','#1f2126','#1e1e2e','#eff1f5','#808080','#000000','#ffffff'].map(background=>({text:JSON.stringify({colors:{'editor.background':background,'editorWidget.background':'#181825','sideBar.background':background,'input.border':'#acb0be',focusBorder:'#268bd240','diffEditor.insertedLineBackground':'#66cc6620'}}),filename:`layers-${background.slice(1)}.json`})),
  {text:'{"colors":{"editor.background":"#123"},"description":"https://fixture.invalid/*literal*/,}"} /* final */',filename:'comments.json'},
  ...['{','{"include":"../base.json"}','{"tokenColors":"tokens.json"}','{"colors":{}}','{"colors":{"editor.background":"red; body {display:none}"}}','{"colors":{"editor.background":"#123"},"tokenColors":[{"settings":{"foreground":"red"}}]}','{"colors":{"editor.background":"#123"},"semanticTokenColors":{"*":""}}','{"name":" ","colors":{"editor.background":"#123"}}','{"colors":{"editor.background":"#123"}} /* unterminated','{"colors":{"editor.background":"#123"},,}','{"colors":{"editor.background":"#123"},"semanticHighlighting":null}'].map(text=>({text,filename:'invalid.json'})),
];
const importCases = imports.map(input => {
  try { const definition = importVSCodeTheme(input.text,input.filename); return {...input, definition, resolved:resolved(definition)}; }
  catch(error) {
    // SAFETY: these fixture conversions throw Error objects with an optional code.
    return {...input,error:(error as {code?:string}).code ?? 'invalid'};
  }
});
const minimal = {
  metadata:{id:'minimal',name:'Minimal',variant:'dark'},
  colors:{primary:{base:'#ffcc66'},surface:{background:'#101010',foreground:'#eeeeee',muted:'#181818',mutedForeground:'#999999',elevated:'#202020'},interactive:{border:'#333333'},status:{error:'#ff6677',warning:'#ffcc66',success:'#88cc66',info:'#66aaff'},syntax:{base:{comment:'#999999',keyword:'#aa88ff',string:'#88cc66',number:'#ffcc66',function:'#66aaff',variable:'#eeeeee',type:'#66dddd',operator:'#ff6677'}}},
};
const definitions: unknown[] = [minimal,
  {...minimal,colors:{...minimal.colors,syntax:{...minimal.colors.syntax,tokens:{class:'#abcdef',className:'#ff00ff',method:'#aa7700',punctuation:'#cccccc',stringEscape:'#aaccff',interface:'#ffccaa',variableProperty:'#fedcba'}}}},
  ...['#ffcc66','#fc6','#ffcc6680','rgb(255 204 102)','rgba(255, 204, 102, 0.5)','hsl(40 100% 70%)','oklch(0.85 0.12 80)','color-mix(in oklab, #ffcc66 40%, transparent)','transparent','#12345','blue','red; } body { display: none','url(https://fixture.invalid/seen.png)','rgb(1 2 3)</style>','image-set("https://fixture.invalid/x" 1x)','@import "https://fixture.invalid/x.css"'].map(base=>({...minimal,colors:{...minimal.colors,primary:{base}}})),
  {...minimal,colors:{...minimal.colors,chat:{userMessage:'#abcdef',unknown:'#123'}}},
  {...minimal,colors:{...minimal.colors,chat:{unknown:42}}},
  {...minimal,config:{fonts:{sans:'"IBM Plex Sans", ui-sans-serif, system-ui'},transitions:{fast:'120ms'}}},
  {...minimal,config:{fonts:{sans:'Inter; } * { background: url(https://fixture.invalid/x)'}}},
];
const themeFiles = (await readdir(resolve(themeRoot,'themes'))).filter(name=>name.endsWith('.json')).sort();
for (const filename of themeFiles) definitions.push(JSON.parse(await readFile(resolve(themeRoot,'themes',filename),'utf8')));
const definitionCases = definitions.map(definition=> {
  try { return {definition,resolved:resolved(definition)}; } catch {return {definition,error:'invalid'};}
});
const sources = ['vscode/import.ts','vscode/adapter.ts','vscode/adapt.ts','color.ts','definition.ts','readableColors.ts','syntax.ts','themes/index.ts','themes/prColors.ts',...themeFiles.map(name=>`themes/${name}`)].map(name=>`packages/ui/src/lib/theme/${name}`);
sources.push('packages/ui/src/lib/agentColorPalette.ts','native/crates/desktop/resources/themes.json');
const hashes: Record<string,string> = {};
for (const path of sources) hashes[path] = createHash('sha256').update(await readFile(resolve(root,path))).digest('hex');
const output = resolve(root,'native/crates/desktop/src/theme_import/fixtures.json');
await writeFile(output,JSON.stringify({sources:hashes,imports:importCases,definitions:definitionCases},null,2)+'\n');
console.log(`Generated ${importCases.length} import and ${definitionCases.length} definition cases from current source into ${output}`);
