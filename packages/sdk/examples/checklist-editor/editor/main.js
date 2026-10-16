(()=>{var S="openchamber.sdk",g=1;var qe=`
:root {
  /* Quiet at rest like the app's own thumbs; hover gives a clear grab target. */
  --oc-scrollbar-thumb: color-mix(in srgb, var(--oc-muted, currentColor) 18%, transparent);
  --oc-scrollbar-thumb-hover: color-mix(in srgb, var(--oc-muted, currentColor) 36%, transparent);
  scrollbar-gutter: stable;
}
* {
  scrollbar-width: thin;
  scrollbar-color: transparent transparent;
}
:hover, [data-oc-scrolling] {
  scrollbar-color: var(--oc-scrollbar-thumb) transparent;
}
/* Chromium's standard scrollbar properties otherwise override its pseudo-elements. */
@supports selector(::-webkit-scrollbar) {
  *, :hover, [data-oc-scrolling] { scrollbar-width: auto; scrollbar-color: auto; }
  ::-webkit-scrollbar { width: 6px; height: 6px; background: transparent; }
  ::-webkit-scrollbar-track { background: transparent; }
  ::-webkit-scrollbar-thumb {
    background: transparent;
    border-radius: 999px;
    min-width: 24px;
    min-height: 24px;
  }
  :hover::-webkit-scrollbar-thumb, [data-oc-scrolling]::-webkit-scrollbar-thumb { background: var(--oc-scrollbar-thumb); }
  ::-webkit-scrollbar-thumb:hover { background: var(--oc-scrollbar-thumb-hover); }
  ::-webkit-scrollbar-corner { background: transparent; }
  ::-webkit-scrollbar-button { display: none; width: 0; height: 0; }
}
@media (forced-colors: active) {
  *, :hover, [data-oc-scrolling] { scrollbar-color: auto; }
  ::-webkit-scrollbar-thumb, ::-webkit-scrollbar-thumb:hover { background: CanvasText; }
}
`;function ze(e){let s=e.documentElement;if(s.hasAttribute("data-oc-scrollbar-activity"))return;s.setAttribute("data-oc-scrollbar-activity","");let r=new WeakMap;e.addEventListener("scroll",(a)=>{let c=a.target===e?s:a.target;if(!(c instanceof Element))return;if(!c.hasAttribute("data-oc-scrolling"))c.setAttribute("data-oc-scrolling","");let m=r.get(c);if(m!==void 0)clearTimeout(m);r.set(c,setTimeout(()=>{r.delete(c),c.removeAttribute("data-oc-scrolling")},1000))},{capture:!0,passive:!0})}var Uo=`(${ze.toString()})(document);`;var je=128,Ke=65536;var xe=128;var be=65536;var zt=/^[a-z][a-z0-9-]{0,39}$/;var Vt=(e)=>Object(e)===e&&!Array.isArray(e),Je=(e,s)=>Object.keys(e).every((r)=>s.includes(r)),Wt=(e)=>String(e)===e&&e.length>=1&&e.length<=60,qt=(e)=>String(e)===e&&e.length>=1&&e.length<=80,jt=(e)=>{if(!Array.isArray(e)||e.length>4)return!1;let s=new Set;for(let r of e){if(!Vt(r)||!zt.test(r.id)||!Wt(r.label))return!1;if(r.disabled!==void 0&&r.disabled!==!0&&r.disabled!==!1)return!1;if(s.has(r.id))return!1;if(s.add(r.id),r.kind==="button"){if(!Je(r,["kind","id","label","disabled"]))return!1;continue}if(r.kind!=="select"||!Je(r,["kind","id","label","value","options","disabled"]))return!1;if(!qt(r.value)||!Array.isArray(r.options)||r.options.length<1||r.options.length>16)return!1;let a=new Set;for(let c of r.options){if(!Vt(c)||!Je(c,["value","label"])||!qt(c.value)||!Wt(c.label)||a.has(c.value))return!1;a.add(c.value)}if(!a.has(r.value))return!1}return!0};var Te=20000000,ve=256;var Ye=(e)=>("bytes"in e)?e.bytes.byteLength:e.content.length,Kt=(e,s)=>{if(e.path!==s.path||e.readOnly!==s.readOnly)return!1;if(e.encoding==="text"||s.encoding==="text")return e.encoding==="text"&&s.encoding==="text"&&e.content===s.content;if(e.bytes.byteLength!==s.bytes.byteLength)return!1;for(let r=0;r<e.bytes.byteLength;r+=1)if(e.bytes[r]!==s.bytes[r])return!1;return!0},_e=(e)=>{let s=e,r=e;return{observe:(a)=>{let c=a!==r;return r=a,{edited:c,dirty:a!==s}},markSaved:(a)=>(s=a,r!==a)}};var Jt=/^[A-Za-z0-9_-]{1,80}$/;var Yt=["left","right","top","bottom"];var Qt=(e,s=0,r={remaining:16000})=>{if(--r.remaining<0||s>=32)return!1;if(e===null)return!0;let a=Array.isArray(e)?e:Object(e)===e?Object.values(e):[];for(let c of a)if(!Qt(c,s+1,r))return!1;return!0},Qe=(e)=>{if(e===null||e===!0||e===!1)return!0;if(Array.isArray(e))return Object.keys(e).every((s)=>/^(0|[1-9]\d*)$/.test(s)&&Number(s)<e.length)&&e.every(Qe);if(Object(e)===e){let s=Object.getPrototypeOf(e);return(s===null||s===Object.prototype)&&Object.values(e).every(Qe)}return String(e)===e||Number.isFinite(e)},Ze=(e)=>{try{if(String(e.id)!==e.id||!Jt.test(e.id)||!Number.isFinite(e.anchor.x)||Math.abs(e.anchor.x)>1e5||!Number.isFinite(e.anchor.y)||Math.abs(e.anchor.y)>1e5||!Number.isFinite(e.anchor.width)||!Number.isFinite(e.anchor.height)||e.anchor.width<=0||e.anchor.height<=0||e.anchor.width>1e5||e.anchor.height>1e5||!Number.isInteger(e.width)||e.width<160||e.width>640||!Number.isInteger(e.height)||e.height<48||e.height>480||e.side!==void 0&&!Yt.includes(e.side)||e.focus!==void 0&&e.focus!==!0&&e.focus!==!1||!Qt(e.data)||!Qe(e.data))return!1;return JSON.stringify(e.data).length<=16000}catch{return!1}};var eo=["file","directory","other","missing"],tt=(e)=>Boolean(e&&"sessionId"in e),ot=(e)=>Boolean(e&&"sent"in e&&!("sessionId"in e));var to=/^[0-9a-f]{7,64}$/i,st=(e)=>to.test(e),ke=500,Ae=32000,oo=16000,so=128,ro=200,no=2000,io=16000,ao=80,lo=200,co=16000;var po=2000;var rt=20000,Re=1024,Ge=2000000;var Ce=64000,Oe=8000,nt=4000;var it=90000;var uo=999,So=1e4,ue=500,go=["HOST_UNAVAILABLE","HOST_TIMEOUT","HOST_REJECTED","DISCONNECTED","DISABLED","BAD_PATH","NO_INTEGRATION","NO_SERVICE","SERVICE_FAILED","REQUEST_FAILED","RESPONSE_TOO_LARGE","NO_SESSION","SESSION_BUSY","NOT_GRANTED","NO_DIRECTORY","NOT_FOUND","FILE_TOO_LARGE","DENIED","NO_MODEL","MODEL_FAILED","UNSUPPORTED"],mo=["stopped","starting","ready","failed"],Lo=new Set(go),Eo=(e)=>Lo.has(e),fo=(e)=>e&&Eo(e)?e:"HOST_REJECTED",Z=(e)=>{if(e===void 0)return!1;if(e===null||e===!0||e===!1)return!0;if(String(e)===e)return!0;if(Number(e)===e)return Number.isFinite(e);if(Array.isArray(e))return e.every(Z);if(Object(e)===e)return Object.values(e).every(Z);return!1},yo=(e)=>Z(e)&&JSON.stringify(e).length<=co,Zt=(e)=>e?.trim().slice(0,lo)??"",ee=(e)=>{let s=e.id.trim().slice(0,so),r=e.title.trim().slice(0,ro),a=e.url.trim().slice(0,no),c=e.text?.trim().slice(0,io),m=e.author?.trim().slice(0,ao),y=e.kind==="pull"?"pull":"issue",E={providerId:e.providerId.trim(),id:s,title:r||s,url:a,kind:y};if(c)E.text=c;if(m)E.author=m;if(y==="pull"){let G=Zt(e.branches?.head),M=Zt(e.branches?.base);if(G&&M)E.branches={head:G,base:M}}if(yo(e.data))E.data=e.data;return E},at=(e)=>{let s=ee(e);if(e.projectId)s.projectId=e.projectId;if(e.navigation)s.navigation=e.navigation;if(e.worktree)s.worktree=e.worktree;return s},lt=(e)=>{let s={text:e.text.trim().slice(0,oo)};if(e.send)s.send=!0;return s},ct=(e)=>{if(e===null||!Number.isFinite(e))return null;return Math.min(uo,Math.max(0,Math.round(e)))},pt=(e)=>{if(!Number.isFinite(e))return 0;return Math.min(So,Math.max(0,Math.ceil(e)))};var te=(e)=>e.length>0&&e.length<=Re&&!e.includes("\x00")&&!e.includes("\\");var Pe=(e)=>{if(!e.startsWith("/")||e.includes("\x00")||e.includes("\\")||e.includes("://"))return!1;if(e.length>po)return!1;return!e.split("/").some((r)=>r==="."||r==="..")},Ho=new Set(mo),ut=(e)=>Boolean(e&&"status"in e&&Ho.has(String(e.status))&&!("body"in e)),we=(e)=>Boolean(e&&"status"in e&&"body"in e&&Number.isInteger(e.status)),dt=(e)=>Boolean(e&&"content"in e&&String(e.content)===e.content),St=(e)=>Boolean(e&&"written"in e&&e.written===!0),gt=(e)=>Boolean(e&&"entries"in e&&Array.isArray(e.entries)),Fo=new Set(eo),mt=(e)=>Boolean(e&&"kind"in e&&"size"in e&&Fo.has(String(e.kind))&&Number.isFinite(e.size)),Et=(e)=>Boolean(e&&"text"in e&&String(e.text)===e.text&&!("status"in e)),No=new Set(["workspace","shells","ready","directory","session","connection","settings","session-lifecycle","item","resolve","action","status-control-event","popover-closed","file-open","file-snapshot","file-saved"]),Bo=(e)=>Object(e)===e?e:null,et=(e)=>String(e)===e&&e.length>0,Do=(e)=>{if(!et(e.id))return null;if(e.ok===!0){let s={channel:S,v:g,type:"result",id:e.id,ok:!0};if(Object(e.payload)===e.payload)s.payload=e.payload;return s}if(e.ok===!1&&et(e.error))return{channel:S,v:g,type:"result",id:e.id,ok:!1,error:e.error,code:fo(et(e.code)?e.code:void 0)};return null},ft=(e)=>{let s=Bo(e);if(!s||s.channel!==S||s.v!==g)return null;if(s.type==="result")return Do(s);if(!No.has(String(s.type))||Object(s.payload)!==s.payload)return null;return s};var ho=(e)=>("key"in e)&&("metaKey"in e)&&("ctrlKey"in e),$o=(e)=>(e.metaKey||e.ctrlKey)&&!e.altKey&&!e.shiftKey&&e.key.toLowerCase()==="s";class l extends Error{code;constructor(e,s){super(s);this.name="HostRequestError",this.code=e}}var xo=()=>Promise.reject(new l("BAD_PATH",'Request path must start with "/" and stay on the declared origin.')),Me=()=>Promise.reject(new l("BAD_PATH",`File path must be 1 to ${Re} characters without NUL or backslash.`)),Ie=(e)=>{if(e===void 0)return;if(Object(e)!==e||Array.isArray(e)||e.scope!==void 0&&e.scope!=="instance"&&e.scope!=="device")throw new l("HOST_REJECTED",'Storage scope must be "instance" or "device".');return e.scope==="device"?"device":void 0},d=(e)=>(e.value+=1,`oc-${e.value}`),yt=(e={})=>{let s=e.target??("window"in globalThis?window:null);if(!s)throw new l("HOST_UNAVAILABLE","No window. connectHost runs in a browser frame.");let r=e.acceptSource??((t)=>t===s.parent),a=e.requestTimeoutMs??rt,c=new Set,m=new Set,y=new Set,E=new Set,G=new Set,M=new Set,re=new Set,Se=new Set,ge=new Set,ne=null,ie=null,me=new Set,Ee=new Set,ae=null,le=null,$e=!1,J=new Map,Y=new Map,Q=new Map,F=!1,u={value:0},p=null,N=null,ce=!1,pe=!1,Mt=null,It=(t)=>{if(!t)return null;return{sessionId:t.id,phase:t.busy?"started":"completed"}},U=(t)=>{s.parent.postMessage(t,"*")},R=(t,o)=>{for(let n of t)try{n(o)}catch(i){console.error(i)}},Ut=(t)=>{if(!(t instanceof MessageEvent))return;if(!r(t.source))return;let o=ft(t.data);if(!o)return;if(o.type==="workspace"){let i=Y.get(o.payload.subscriptionId);if(i)R([i],o.payload.snapshot);return}if(o.type==="shells"){let i=Q.get(o.payload.subscriptionId);if(i)R([i],o.payload.snapshot);return}if(o.type==="ready"){if(p=o.payload,N=It(o.payload.session),R(c,o.payload),R(m,o.payload.directory),R(y,o.payload.session),N)R(E,N);R(G,o.payload.connection),R(M,o.payload.settings),R(re,o.payload.item);return}if(o.type==="directory"){if(p)p={...p,directory:o.payload.directory};R(m,o.payload.directory);return}if(o.type==="session"){if(p)p={...p,session:o.payload.session};if(!o.payload.session)N=null;else if(N?.sessionId!==o.payload.session.id)N=It(o.payload.session);R(y,o.payload.session);return}if(o.type==="session-lifecycle"){N=o.payload,R(E,o.payload);return}if(o.type==="connection"){if(p)p={...p,connection:o.payload.connection};R(G,o.payload.connection);return}if(o.type==="settings"){if(p)p={...p,settings:o.payload.settings};R(M,o.payload.settings);return}if(o.type==="item"){if(p)p={...p,item:o.payload.item};R(re,o.payload.item);return}if(o.type==="status-control-event"){R(Se,o.payload);return}if(o.type==="popover-closed"){R(ge,o.payload);return}if(o.type==="action"){let i=(f)=>{if(!F)U({channel:S,v:g,type:"action-result",id:o.id,payload:f})},h=ie;if(!h){i({ok:!1,error:"This extension does not handle background actions."});return}Promise.resolve().then(()=>h(o.payload)).then(()=>i({ok:!0}),(f)=>{let k=(f instanceof Error?f.message:String(f)).trim();i({ok:!1,error:(k||"Action failed.").slice(0,ue)})});return}if(o.type==="file-open"){let i=o.payload;if(le&&Kt(le,i))return;le=i,R(me,o.payload);return}if(o.type==="file-saved"){R(Ee,o.payload.version);return}if(o.type==="file-snapshot"){let i=(k)=>{if(!F)U({channel:S,v:g,type:"file-snapshot-result",id:o.id,payload:k})},h=(k)=>i({error:(k.trim()||"Could not read the edited file.").slice(0,ue)}),f=ae;if(!f){h("This extension does not edit files.");return}Promise.resolve().then(()=>f(o.payload.purpose)).then((k)=>{if(Ye(k)>Te){h(`The file is over ${Te} ${"bytes"in k?"bytes":"characters"}.`);return}if(k.version.length>ve){h(`The snapshot version is over ${ve} characters.`);return}i({snapshot:"bytes"in k?{bytes:k.bytes,version:k.version}:{content:k.content,version:k.version}})},(k)=>h(k instanceof Error?k.message:String(k)));return}if(o.type==="resolve"){let i=(f)=>{U({channel:S,v:g,type:"resolve-result",id:o.id,payload:f})},h=ne;if(!h){i({error:"This extension does not resolve commands."});return}Promise.resolve().then(()=>h(o.payload)).then((f)=>i({item:f?ee(f):null}),(f)=>{let k=(f instanceof Error?f.message:String(f)).trim();i({error:(k||"Command failed.").slice(0,ue)})});return}let n=J.get(o.id);if(!n)return;if(clearTimeout(n.timer),J.delete(o.id),o.ok){n.resolve(o.payload);return}n.reject(new l(o.code,o.error))};s.addEventListener("message",Ut),U({channel:S,v:g,type:"hello"});let P=(t,o=a)=>{if(F||s.parent===s)return Promise.reject(new l("HOST_UNAVAILABLE","No host frame. This page is not in an iframe."));return new Promise((n,i)=>{let h=setTimeout(()=>{J.delete(t.id),i(new l("HOST_TIMEOUT","Host did not answer in time."))},o);J.set(t.id,{resolve:n,reject:i,timer:h}),U(t)})},v=(t)=>P(t).then(()=>{return}),_={channel:S,v:g},Xe=(t)=>{if(!F&&s.parent!==s)U(t)},Lt=()=>Xe({..._,type:"file-save"}),Ht=(t)=>{if(!ho(t)||!$o(t))return;t.preventDefault(),Lt()},Ft=(t)=>{if(!ho(t)||t.key!=="Escape"||p?.surface!=="popover"||!p.popover)return;t.preventDefault(),$t(p.popover.id,"escape").catch(()=>{return})},fe=()=>{let t=p?.surface==="popover"?p.popover:void 0;if(!t)return;let o=ce||pe;if(Mt===o)return;Mt=o,Xt(t.id,o).catch(()=>{return})},ye=()=>{if(p?.surface!=="popover"||ce)return;ce=!0,fe()},Nt=(t)=>{if(p?.surface!=="popover"||t.relatedTarget!==null||!ce)return;ce=!1,fe()},Bt=()=>{if(p?.surface!=="popover"||pe)return;pe=!0,fe()},Dt=()=>{if(p?.surface!=="popover"||!pe)return;pe=!1,fe()},H=(t,o=1024)=>{if(!t.trim()||t.length>o)throw new l("HOST_REJECTED",`Identity must contain 1 to ${o} characters.`)},Ve=async(t)=>{if(t.kind!=="projects")H(t.projectId);let o=await P({..._,type:"workspace-read",id:d(u),payload:t});if(!o||!("kind"in o)||!("state"in o)||o.kind!==t.kind)throw new l("HOST_REJECTED","Host did not return workspace data.");return o},We=async(t,o)=>{if(t.kind!=="projects")H(t.projectId);let n=d(u);Y.set(n,o);try{await v({..._,type:"workspace-subscribe",id:d(u),payload:{subscriptionId:n,query:t}})}catch(i){if(Y.delete(n),!F)U({..._,type:"workspace-unsubscribe",id:d(u),payload:{subscriptionId:n}});throw i}return()=>{if(!Y.delete(n)||F)return;U({..._,type:"workspace-unsubscribe",id:d(u),payload:{subscriptionId:n}})}},Io=async(t,o)=>{if(t.kind==="session")H(t.sessionId);if(t.kind==="project")H(t.projectId);let n=d(u);Q.set(n,o);try{await v({..._,type:"shells-subscribe",id:d(u),payload:{subscriptionId:n,scope:t}})}catch(i){if(Q.delete(n),!F)U({..._,type:"shells-unsubscribe",id:d(u),payload:{subscriptionId:n}});throw i}return()=>{if(!Q.delete(n)||F)return;U({..._,type:"shells-unsubscribe",id:d(u),payload:{subscriptionId:n}})}},he=async(t)=>{if("key"in t&&(t.key.length===0||t.key.length>je))throw new l("HOST_REJECTED","Storage key must contain 1 to 128 characters.");if(t.op==="set"&&!Z(t.value))throw new l("HOST_REJECTED","Storage values must be JSON.");if(t.op==="set"&&new TextEncoder().encode(JSON.stringify(t.value)).length>Ke)throw new l("HOST_REJECTED","Storage value exceeds 64 KiB.");if(t.scope==="device"&&p?.features?.deviceStorage!==!0)throw new l("UNSUPPORTED","This host does not support device storage.");let o=await P({..._,type:"storage",id:d(u),payload:t});if(!o||!("storage"in o)||o.op!==t.op)throw new l("HOST_REJECTED","Host did not return storage data.");return o},$t=(t,o)=>p?.features?.popovers!==!0&&!(p?.surface==="popover"&&p.popover?.id===t)?Promise.reject(new l("UNSUPPORTED","This host does not support popovers.")):v({..._,type:"popover-close",id:d(u),payload:o?{id:t,reason:o}:{id:t}}),Xt=(t,o)=>p?.features?.popovers!==!0&&!(p?.surface==="popover"&&p.popover?.id===t)?Promise.reject(new l("UNSUPPORTED","This host does not support popovers.")):v({..._,type:"popover-anchor",id:d(u),payload:{id:t,active:o}});return s.addEventListener("keydown",Ft,!0),s.addEventListener("pointerover",ye),s.addEventListener("pointermove",ye),s.addEventListener("pointerout",Nt),s.addEventListener("focus",Bt),s.addEventListener("blur",Dt),{onAction:(t)=>(ie=t,()=>{if(ie===t)ie=null}),listProjects:async()=>{let t=await Ve({kind:"projects"});if(t.kind!=="projects")throw new l("HOST_REJECTED","Expected projects.");return t},listWorktrees:async(t)=>{let o=await Ve({kind:"worktrees",projectId:t});if(o.kind!=="worktrees")throw new l("HOST_REJECTED","Expected worktrees.");return o},listSessions:async(t)=>{let o=await Ve({kind:"sessions",projectId:t});if(o.kind!=="sessions")throw new l("HOST_REJECTED","Expected sessions.");return o},onProjects:(t)=>We({kind:"projects"},(o)=>{if(o.kind==="projects")t(o)}),onWorktrees:(t,o)=>We({kind:"worktrees",projectId:t},(n)=>{if(n.kind==="worktrees")o(n)}),onSessions:(t,o)=>We({kind:"sessions",projectId:t},(n)=>{if(n.kind==="sessions")o(n)}),onRunningShells:(t,o)=>Io(t,o),readShellOutput:async(t,o)=>{H(t,xe);let n=o?.cursor;if(n!==void 0&&(!Number.isInteger(n)||n<0))throw new l("HOST_REJECTED","Shell output cursor must be a non-negative integer.");let i=o?.tailBytes;if(i!==void 0&&(!Number.isInteger(i)||i<1||i>be))throw new l("HOST_REJECTED",`Shell output tailBytes must be an integer between 1 and ${be}.`);let f=await P({..._,type:"shell-output",id:d(u),payload:{shellId:t,cursor:n,tailBytes:i}});if(!f||typeof f.output!=="string"||!Number.isFinite(f.cursor))throw new l("HOST_REJECTED","Host did not return shell output.");return f},stopShell:async(t)=>{if(H(t,xe),(await P({..._,type:"shell-stop",id:d(u),payload:{shellId:t}}))?.stopped!==!0)throw new l("HOST_REJECTED","Host did not stop the shell.");return{stopped:!0}},openSession:async(t)=>{H(t),await v({..._,type:"open-session",id:d(u),payload:{sessionId:t}})},storage:{get:async(t,o)=>{let n=Ie(o),i={op:"get",key:t};if(n)i.scope=n;let h=await he(i);return h.op==="get"&&h.found?h.value:void 0},set:async(t,o,n)=>{let i=Ie(n),h={op:"set",key:t,value:o};if(i)h.scope=i;await he(h)},delete:async(t,o)=>{let n=Ie(o),i={op:"delete",key:t};if(n)i.scope=n;await he(i)},keys:async(t)=>{let o=Ie(t),n={op:"keys"};if(o)n.scope=o;let i=await he(n);if(i.op!=="keys")throw new l("HOST_REJECTED","Expected storage keys.");return i.keys}},onReady:(t)=>{if(c.add(t),p)t(p);return()=>{c.delete(t)}},onDirectory:(t)=>{if(m.add(t),p)t(p.directory);return()=>{m.delete(t)}},onSession:(t)=>{if(y.add(t),p)t(p.session);return()=>{y.delete(t)}},onSessionLifecycle:(t)=>{if(E.add(t),N)t(N);return()=>{E.delete(t)}},onConnection:(t)=>{if(G.add(t),p)t(p.connection);return()=>{G.delete(t)}},onSettings:(t)=>{if(M.add(t),p)t(p.settings);return()=>{M.delete(t)}},onItem:(t)=>{if(re.add(t),p)t(p.item);return()=>{re.delete(t)}},onStatusControl:(t)=>(Se.add(t),()=>{Se.delete(t)}),onPopoverClosed:(t)=>(ge.add(t),()=>{ge.delete(t)}),setStatusControls:(t)=>{if(p?.features?.statusControls!==!0)return Promise.reject(new l("UNSUPPORTED","This host does not support status controls."));if(!jt(t))return Promise.reject(new l("HOST_REJECTED","Status controls must have unique bounded ids and valid selected options."));return v({..._,type:"status-controls",id:d(u),payload:{controls:t}})},openPopover:(t)=>{if(p?.features?.popovers!==!0)return Promise.reject(new l("UNSUPPORTED","This host does not support popovers."));if(!Ze(t))return Promise.reject(new l("HOST_REJECTED","Popover requests need bounded geometry and JSON data."));return v({..._,type:"popover-open",id:d(u),payload:t})},closePopover:$t,setPopoverAnchorActive:Xt,onResolve:(t)=>(ne=t,()=>{if(ne===t)ne=null}),toast:(t)=>{let o=t.message.trim();if(!o||o.length>ke)return Promise.reject(new l("HOST_REJECTED",`Toast message must contain 1 to ${ke} characters.`));if(t.copy&&t.copy!==!0&&(!t.copy.text.length||t.copy.text.length>Ae))return Promise.reject(new l("HOST_REJECTED",`Toast copy text must contain 1 to ${Ae} characters.`));return v({channel:S,v:g,type:"toast",id:d(u),payload:{...t,message:o}})},openUrl:(t)=>v({channel:S,v:g,type:"open-url",id:d(u),payload:{url:t}}),openCommit:(t)=>st(t)?v({channel:S,v:g,type:"open-commit",id:d(u),payload:{sha:t}}):Promise.reject(new l("HOST_REJECTED","Commit id must be 7 to 64 hex characters.")),openSurface:(t)=>v({channel:S,v:g,type:"open-surface",id:d(u),payload:{surfaceId:t}}),writeClipboard:(t)=>v({channel:S,v:g,type:"clipboard-write",id:d(u),payload:{text:t}}),compose:(t)=>v({channel:S,v:g,type:"compose",id:d(u),payload:t}),attach:(t)=>v({channel:S,v:g,type:"attach",id:d(u),payload:ee(t)}),startSession:async(t)=>{if(t.projectId!==void 0)H(t.projectId);let o=t.worktree;if(o&&o!==!0)if(o.kind==="existing")H(o.directory);else{if(o.name!==void 0)H(o.name,200);if(o.baseBranch!==void 0)H(o.baseBranch,200)}let n=await P({channel:S,v:g,type:"start-session",id:d(u),payload:at(t)},e.requestTimeoutMs??180000);if(!tt(n))throw new l("HOST_REJECTED","Host did not return a session.");return n},prompt:(t)=>P({channel:S,v:g,type:"prompt",id:d(u),payload:lt(t)}).then((o)=>{if(!ot(o))throw new l("HOST_REJECTED","Host did not return a prompt result.");return o}),sessionLink:(t)=>v({channel:S,v:g,type:"session-link",id:d(u),payload:ee(t)}),close:()=>v({channel:S,v:g,type:"close",id:d(u)}),oauthStart:()=>v({channel:S,v:g,type:"oauth-start",id:d(u)}),oauthDisconnect:()=>v({channel:S,v:g,type:"oauth-disconnect",id:d(u)}),request:(t)=>(Pe(t.path)?P({channel:S,v:g,type:"request",id:d(u),payload:t}):xo()).then((o)=>{if(!we(o))throw new l("HOST_REJECTED","Host request result was empty.");return o}),serviceRequest:(t)=>(Pe(t.path)?P({channel:S,v:g,type:"service-request",id:d(u),payload:t}):xo()).then((o)=>{if(!we(o))throw new l("HOST_REJECTED","Host service request result was empty.");return o}),serviceStatus:()=>P({channel:S,v:g,type:"service-status",id:d(u)}).then((t)=>{if(!ut(t))throw new l("HOST_REJECTED","Host did not return service status.");return t}),readFile:(t)=>(te(t)?P({channel:S,v:g,type:"file-read",id:d(u),payload:{path:t}}):Me()).then((o)=>{if(!dt(o))throw new l("HOST_REJECTED","Host did not return file content.");return o}),writeFile:(t,o)=>{if(!te(t))return Me();if(o.length>Ge)return Promise.reject(new l("FILE_TOO_LARGE",`Content is over ${Ge} characters.`));return P({channel:S,v:g,type:"file-write",id:d(u),payload:{path:t,content:o}}).then((n)=>{if(!St(n))throw new l("HOST_REJECTED","Host did not confirm the write.");return n})},listDir:(t)=>(te(t)?P({channel:S,v:g,type:"file-list",id:d(u),payload:{path:t}}):Me()).then((o)=>{if(!gt(o))throw new l("HOST_REJECTED","Host did not return directory entries.");return o}),stat:(t)=>(te(t)?P({channel:S,v:g,type:"file-stat",id:d(u),payload:{path:t}}):Me()).then((o)=>{if(!mt(o))throw new l("HOST_REJECTED","Host did not return file status.");return o}),generate:(t)=>{let o=t.prompt.trim(),n=t.system?.trim();if(o.length===0||o.length>Ce)return Promise.reject(new l("HOST_REJECTED",`Prompt must be 1 to ${Ce} characters.`));if(n!==void 0&&(n.length===0||n.length>Oe))return Promise.reject(new l("HOST_REJECTED",`System prompt must be 1 to ${Oe} characters.`));let i=t.maxOutputTokens===void 0?void 0:Math.min(nt,Math.max(1,Math.floor(t.maxOutputTokens)));if(i!==void 0&&!Number.isFinite(i))return Promise.reject(new l("HOST_REJECTED","maxOutputTokens must be a number."));let h={prompt:o};if(n!==void 0)h.system=n;if(i!==void 0)h.maxOutputTokens=i;return P({channel:S,v:g,type:"generate",id:d(u),payload:h},e.requestTimeoutMs??it).then((f)=>{if(!Et(f))throw new l("HOST_REJECTED","Host did not return generated text.");return f})},setBadge:(t)=>v({channel:S,v:g,type:"badge",id:d(u),payload:{count:ct(t)}}),setHeight:(t)=>v({channel:S,v:g,type:"resize",id:d(u),payload:{height:pt(t)}}),onFileOpen:(t)=>{if(me.add(t),!$e)$e=!0,s.addEventListener("keydown",Ht,!0);if(le)t(le);return()=>{me.delete(t)}},onFileSnapshot:(t)=>(ae=t,()=>{if(ae===t)ae=null}),onFileSaved:(t)=>(Ee.add(t),()=>{Ee.delete(t)}),reportFileChange:(t)=>Xe({..._,type:"file-change",payload:{dirty:t.dirty,edited:t.edited}}),requestFileSave:Lt,reportFileUnsupported:()=>Xe({..._,type:"file-unsupported"}),dispose:()=>{for(let t of Y.keys())U({..._,type:"workspace-unsubscribe",id:d(u),payload:{subscriptionId:t}});Y.clear();for(let t of Q.keys())U({..._,type:"shells-unsubscribe",id:d(u),payload:{subscriptionId:t}});if(Q.clear(),F=!0,ne=null,ie=null,ae=null,me.clear(),Ee.clear(),$e)s.removeEventListener("keydown",Ht,!0);s.removeEventListener("message",Ut),s.removeEventListener("keydown",Ft,!0),s.removeEventListener("pointerover",ye),s.removeEventListener("pointermove",ye),s.removeEventListener("pointerout",Nt),s.removeEventListener("focus",Bt),s.removeEventListener("blur",Dt);for(let t of J.values())clearTimeout(t.timer),t.reject(new l("HOST_UNAVAILABLE","Host client was disposed."));J.clear(),c.clear(),m.clear(),y.clear(),E.clear(),G.clear(),M.clear(),re.clear(),Se.clear(),ge.clear()}}};var bo=["browser.open","browser.snapshot","browser.click","browser.type","browser.scroll","browser.back","browser.forward","browser.inspect","browser.capture","browser.resize"];var bs=new Set(bo);var To=["none","agent","user"];var vs=new Set(To);var Xo=[["--oc-bg","background"],["--oc-elevated","elevated"],["--oc-fg","foreground"],["--oc-muted","muted"],["--oc-subtle","subtle"],["--oc-border","border"],["--oc-hover","hover"],["--oc-selection","selection"],["--oc-focus","focus"],["--oc-primary","primary"],["--oc-muted-surface","mutedSurface"],["--oc-elevated-fg","elevatedForeground"],["--oc-active","active"],["--oc-selection-fg","selectionForeground"],["--oc-primary-fg","primaryForeground"],["--oc-primary-text","primaryText"],["--oc-success-text","successText"],["--oc-warning-text","warningText"],["--oc-error-text","errorText"],["--oc-info-text","infoText"],["--oc-success","success"],["--oc-warning","warning"],["--oc-error","error"],["--oc-info","info"],["--oc-font","font"],["--oc-mono","mono"],["--oc-radius","radius"],["--surface-background","background"],["--surface-elevated","elevated"],["--surface-foreground","foreground"],["--surface-muted-foreground","muted"],["--surface-subtle","subtle"],["--interactive-border","border"],["--interactive-hover","hover"],["--interactive-selection","selection"],["--interactive-focus-ring","focus"],["--primary","primary"],["--surface-muted","mutedSurface"],["--surface-elevated-foreground","elevatedForeground"],["--interactive-active","active"],["--interactive-selection-foreground","selectionForeground"],["--primary-foreground","primaryForeground"],["--primary-text","primaryText"],["--success-text","successText"],["--warning-text","warningText"],["--error-text","errorText"],["--info-text","infoText"],["--status-success","success"],["--status-warning","warning"],["--status-error","error"],["--status-info","info"],["--font-sans","font"],["--font-mono","mono"],["--radius","radius"]],Vo=[["--oc-syntax-keyword","--syntax-keyword","syntaxKeyword"],["--oc-syntax-string","--syntax-string","syntaxString"],["--oc-syntax-number","--syntax-number","syntaxNumber"],["--oc-syntax-function","--syntax-function","syntaxFunction"],["--oc-syntax-type","--syntax-type","syntaxType"],["--oc-syntax-comment","--syntax-comment","syntaxComment"],["--oc-syntax-variable","--syntax-variable","syntaxVariable"],["--oc-syntax-operator","--syntax-operator","syntaxOperator"]],vo=(e,s)=>{s.style.colorScheme=e.mode;for(let[r,a]of Xo)s.style.setProperty(r,e.tokens[a]);for(let[r,a,c]of Vo){let m=e.tokens[c]??"";s.style.setProperty(r,m),s.style.setProperty(a,m)}s.style.setProperty("font-family",e.tokens.font),s.style.setProperty("font-size","0.875rem"),s.style.setProperty("line-height","1.45"),s.style.setProperty("color",e.tokens.foreground)},ht=(e,s)=>{if(vo(e.theme,s),s.dataset)s.dataset.ocSurface=e.surface,s.dataset.ocTheme=e.theme.mode};var Wo={"surface-background":"bg","surface-elevated":"elevated","surface-elevated-foreground":"elevated-fg","surface-foreground":"fg","surface-muted-foreground":"muted","surface-muted":"muted-surface","surface-subtle":"subtle","interactive-border":"border","interactive-hover":"hover","interactive-active":"active","interactive-selection":"selection","interactive-selection-foreground":"selection-fg","interactive-focus-ring":"focus",primary:"primary","primary-foreground":"primary-fg","primary-text":"primary-text","success-text":"success-text","warning-text":"warning-text","error-text":"error-text","info-text":"info-text","status-success":"success","status-warning":"warning","status-error":"error","status-info":"info","font-sans":"font","font-mono":"mono",radius:"radius"},T=(e,s)=>`var(--${e}, var(--oc-${Wo[e]}, ${s}))`,q=T("surface-background","transparent"),de=T("surface-elevated","transparent"),oe=T("surface-elevated-foreground","inherit"),L=T("surface-foreground","inherit"),x=T("surface-muted-foreground","gray"),qo=T("surface-muted","transparent"),B=T("interactive-border","currentColor"),w=T("interactive-hover","transparent"),z=T("interactive-active","transparent"),xt=T("interactive-selection","transparent"),bt=T("interactive-selection-foreground","inherit"),D=T("primary","currentColor"),Tt=T("primary-text","inherit"),vt=T("error-text","inherit"),zo=T("font-sans","inherit"),_t=T("font-mono","monospace"),Ro=T("radius","9px"),kt=`calc(${Ro} - 2px)`,_o=`calc(${Ro} - 4px)`,b=(e,s,r="transparent")=>`color-mix(in srgb, ${e} ${s}%, ${r})`,ko=`background-image: linear-gradient(${w}, ${w});`,At=(e)=>`box-shadow: inset 0 0 0 1px ${e}, inset 0 1px 2px 0 rgb(0 0 0 / 0.05);`,Rt=`box-shadow: inset 0 0 0 1px ${b(L,10)}, 0 1px 2px 0 rgb(0 0 0 / 0.12);`,Ao="box-shadow: inset 0 1px 0 0 rgb(255 255 255 / 0.12), 0 1px 2px 0 rgb(0 0 0 / 0.14);",Ue=(e)=>{let s=T(`status-${e}`,"currentColor");return`
.oc-sdk[data-tone="${e}"], .oc-sdk [data-tone="${e}"] { --oc-sdk-tone: ${s}; --oc-sdk-tone-text: ${T(`${e}-text`,"inherit")}; }`},jo=`
.oc-sdk { box-sizing: border-box; font-size: 0.875rem; line-height: 1.45; }
.oc-sdk *, .oc-sdk *::before, .oc-sdk *::after { box-sizing: border-box; }
/* :where() keeps the reset at zero specificity so every primitive class below overrides it. */
:where(.oc-sdk) :where(button, input, textarea), :where(button.oc-sdk, input.oc-sdk, textarea.oc-sdk) { font: inherit; color: inherit; margin: 0; }
:where(.oc-sdk) :where(button), :where(button.oc-sdk) { cursor: pointer; background: none; border: 0; padding: 0; }
.oc-sdk button:disabled, button.oc-sdk:disabled, .oc-sdk[aria-disabled="true"], .oc-sdk [aria-disabled="true"] { opacity: .5; pointer-events: none; }

.oc-sdk-btn { display: inline-flex; align-items: center; justify-content: center; gap: 8px; height: 32px; padding: 0 12px; border: 1px solid transparent; font-size: 0.875rem; font-weight: 500; line-height: 1; white-space: nowrap; }
.oc-sdk-btn[data-size="sm"] { gap: 6px; height: 28px; padding: 0 10px; font-size: 0.8125rem; }
.oc-sdk-btn[data-size="xs"] { gap: 4px; height: 24px; padding: 0 8px; font-size: 0.75rem; }
.oc-sdk-btn[data-loading="true"] { opacity: .5; pointer-events: none; }
.oc-sdk-btn > .oc-sdk-spinner-ring { width: 14px; height: 14px; }

.oc-sdk-field { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.oc-sdk-field-label { font-size: 0.8125rem; font-weight: 500; }
.oc-sdk-field-note { font-size: 0.75rem; }
.oc-sdk-input { display: block; width: 100%; min-width: 0; height: 32px; padding: 0 10px; border: 0; font-size: 0.875rem; line-height: 1.45; appearance: none; }
textarea.oc-sdk-input { height: auto; padding: 8px 10px; resize: vertical; }

.oc-sdk-search { position: relative; min-width: 0; }
.oc-sdk-search .oc-sdk-input { padding-left: 34px; padding-right: 34px; }
.oc-sdk-search-icon { position: absolute; left: 11px; top: 50%; transform: translateY(-50%); pointer-events: none; }
.oc-sdk-search-clear { position: absolute; right: 6px; top: 50%; transform: translateY(-50%); display: none; align-items: center; justify-content: center; width: 24px; height: 24px; }
.oc-sdk-search[data-active="true"] .oc-sdk-search-clear { display: inline-flex; }

.oc-sdk-select { position: relative; display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.oc-sdk-trigger { display: inline-flex; align-items: center; gap: 6px; align-self: flex-start; width: auto; max-width: 100%; min-width: 0; height: 28px; padding: 0 10px; border: 1px solid transparent; font-size: 0.8125rem; text-align: left; }
.oc-sdk-trigger-value { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.oc-sdk-trigger-chevron { flex: 0 0 auto; }
.oc-sdk-popup { position: fixed; z-index: 50; display: flex; flex-direction: column; gap: 2px; min-width: 160px; max-width: calc(100vw - 16px); max-height: min(320px, calc(100vh - 16px)); overflow: auto; padding: 4px; border: 1px solid transparent; }
.oc-sdk-popup-search { flex: 0 0 auto; padding: 2px 2px 4px; }
.oc-sdk-popup-search .oc-sdk-input { height: 28px; font-size: 0.8125rem; }
.oc-sdk-option { display: flex; align-items: center; gap: 8px; width: 100%; padding: 6px 8px; font-size: 0.8125rem; text-align: left; }
.oc-sdk-option-label { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.oc-sdk-option-hint { flex: 0 0 auto; font-size: 0.75rem; }
.oc-sdk-option-check { flex: 0 0 auto; display: inline-flex; width: 14px; visibility: hidden; }
.oc-sdk-option[aria-selected="true"] > .oc-sdk-option-check { visibility: visible; }
.oc-sdk-popup-empty { padding: 8px; font-size: 0.8125rem; }

.oc-sdk-check { display: inline-flex; align-items: flex-start; gap: 8px; width: 100%; text-align: left; }
.oc-sdk-check-box { flex: 0 0 auto; display: inline-flex; align-items: center; justify-content: center; width: 14px; height: 14px; margin-top: 3px; border: 1px solid transparent; }
.oc-sdk-check-box > svg { display: none; }
.oc-sdk-check[aria-checked="true"] .oc-sdk-check-box > svg { display: block; }
.oc-sdk-check-thumb { flex: 0 0 auto; position: relative; width: 36px; height: 20px; }
.oc-sdk-check-thumb::after { content: ""; position: absolute; top: 2px; left: 2px; width: 16px; height: 16px; }
.oc-sdk-check-text { display: flex; flex-direction: column; min-width: 0; }
.oc-sdk-check-label { font-size: 0.875rem; }
.oc-sdk-check-desc { font-size: 0.75rem; }

.oc-sdk-tabs { display: inline-flex; gap: 2px; padding: 2px; max-width: 100%; overflow: auto; }
.oc-sdk-tab { display: inline-flex; align-items: center; gap: 6px; height: 28px; padding: 0 10px; border: 1px solid transparent; font-size: 0.8125rem; font-weight: 500; white-space: nowrap; }
.oc-sdk-tab-count { font-size: 0.75rem; font-variant-numeric: tabular-nums; }

.oc-sdk-badge { display: inline-flex; align-items: center; padding: 1px 6px; font-size: 11px; font-weight: 500; line-height: 16px; white-space: nowrap; }

.oc-sdk-list { display: flex; flex-direction: column; gap: 1px; min-width: 0; }
.oc-sdk-row { display: flex; align-items: center; gap: 8px; width: 100%; padding: 6px 8px; text-align: left; }
.oc-sdk-row-lead { flex: 0 0 auto; width: 64px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 0.75rem; }
.oc-sdk-row-main { flex: 1 1 auto; min-width: 0; display: flex; flex-direction: column; }
.oc-sdk-row-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.oc-sdk-row-sub { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 0.75rem; }
.oc-sdk-row-meta { flex: 0 0 auto; font-size: 0.75rem; font-variant-numeric: tabular-nums; }
.oc-sdk-list-empty { padding: 16px 8px; text-align: center; font-size: 0.8125rem; }

.oc-sdk-empty { display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 4px; padding: 40px 16px; text-align: center; }
.oc-sdk-empty-title { margin: 0; font-size: 0.8125rem; font-weight: 600; }
.oc-sdk-empty-body { margin: 0; max-width: 32rem; font-size: 0.8125rem; }
.oc-sdk-empty-action { margin-top: 12px; }

@keyframes oc-sdk-spin { to { transform: rotate(360deg); } }
.oc-sdk-spinner { display: inline-flex; align-items: center; gap: 8px; font-size: 0.8125rem; }
.oc-sdk-spinner-ring { width: 16px; height: 16px; border: 2px solid transparent; animation: oc-sdk-spin .8s linear infinite; }
.oc-sdk-spinner[data-size="sm"] .oc-sdk-spinner-ring { width: 12px; height: 12px; }

.oc-sdk-banner { display: flex; align-items: flex-start; gap: 12px; padding: 8px 12px; border: 1px solid transparent; }
.oc-sdk-banner-text { flex: 1 1 auto; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.oc-sdk-banner-title { font-size: 0.8125rem; font-weight: 500; }
.oc-sdk-banner-body { font-size: 0.8125rem; }
.oc-sdk-banner-action { flex: 0 0 auto; }

.oc-sdk-separator { display: flex; align-items: center; gap: 8px; width: 100%; margin: 8px 0; font-size: 0.75rem; }
.oc-sdk-separator::before, .oc-sdk-separator::after { content: ""; flex: 1 1 auto; height: 1px; }
.oc-sdk-separator[data-labeled="false"]::after { display: none; }
.oc-sdk-popup > .oc-sdk-separator { margin: 4px 0; }

.oc-sdk-progress { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.oc-sdk-progress-label { display: flex; justify-content: space-between; font-size: 0.75rem; font-variant-numeric: tabular-nums; }
.oc-sdk-progress-track { height: 6px; overflow: hidden; }
.oc-sdk-progress-fill { height: 100%; transform-origin: left; transition: transform 200ms ease-out; }

.oc-sdk-menu { position: relative; display: inline-flex; }

.oc-sdk-text { white-space: pre-wrap; overflow-wrap: anywhere; }
.oc-sdk-text img { display: block; max-width: 100%; margin: 8px 0; }
`,Go=`
.oc-sdk { color: ${L}; font-family: ${zo}; }
.oc-sdk :focus-visible, .oc-sdk:focus-visible { outline: none; }
.oc-sdk :where(button, a[href], [role="button"], [role="tab"], [role="option"]):focus-visible, button.oc-sdk:focus-visible { ${ko} }
.oc-sdk-mono { font-family: ${_t}; }
.oc-sdk-muted { color: ${x}; }
${Ue("success")}${Ue("warning")}${Ue("error")}${Ue("info")}
.oc-sdk[data-tone="primary"], .oc-sdk [data-tone="primary"] { --oc-sdk-tone: ${D}; --oc-sdk-tone-text: ${Tt}; }

.oc-sdk-btn { border-radius: ${kt}; transition: background 150ms ease-out, color 150ms ease-out; }
.oc-sdk-btn[data-size="xs"] { border-radius: ${_o}; }
.oc-sdk-btn[data-variant="default"] { color: ${Tt}; background: ${b(D,10,q)}; border-color: ${b(D,12)}; ${Ao} }
.oc-sdk-btn[data-variant="default"]:hover { background: ${b(D,16,q)}; }
.oc-sdk-btn[data-variant="default"]:active { background: ${b(D,22,q)}; box-shadow: none; }
.oc-sdk-btn[data-variant="secondary"] { background: ${qo}; color: var(--oc-fg); }
.oc-sdk-btn[data-variant="secondary"]:hover { background-image: linear-gradient(${w}, ${w}); }
.oc-sdk-btn[data-variant="secondary"]:active { background-image: linear-gradient(${z}, ${z}); }
.oc-sdk-btn[data-variant="outline"] { background: ${de}; color: ${oe}; border-color: transparent; ${Rt} }
.oc-sdk-btn[data-variant="outline"]:hover { background-image: linear-gradient(${w}, ${w}); }
.oc-sdk-btn[data-variant="outline"]:active { background-image: linear-gradient(${z}, ${z}); box-shadow: inset 0 0 0 1px ${b(L,10)}; }
.oc-sdk-btn[data-variant="ghost"] { background: transparent; }
.oc-sdk-btn[data-variant="ghost"]:hover { background: ${w}; }
.oc-sdk-btn[data-variant="ghost"]:active { background: ${z}; }
.oc-sdk-btn[data-variant="destructive"] { --oc-sdk-tone: ${T("status-error","red")}; color: ${vt}; background: ${b("var(--oc-sdk-tone)",7,q)}; border-color: ${b("var(--oc-sdk-tone)",12)}; ${Ao} }
.oc-sdk-btn[data-variant="destructive"]:hover { background: ${b("var(--oc-sdk-tone)",9,q)}; }
.oc-sdk-btn[data-variant="destructive"]:active { background: ${b("var(--oc-sdk-tone)",11,q)}; box-shadow: none; }

.oc-sdk-field-note { color: ${x}; }
.oc-sdk-field[data-invalid="true"] .oc-sdk-field-note { color: ${vt}; }
.oc-sdk-input { border-radius: ${kt}; background: ${de}; color: ${oe}; ${At(b(L,10))} transition: background 150ms ease-out; }
.oc-sdk-input::placeholder { color: ${x}; }
.oc-sdk-input:hover:not(:focus) { background-image: linear-gradient(${w}, ${w}); }
.oc-sdk-input:focus, .oc-sdk-input:focus-visible { ${At(b(L,26))} }
.oc-sdk-field[data-invalid="true"] .oc-sdk-input, .oc-sdk-field[data-invalid="true"] .oc-sdk-input:focus { ${At(T("status-error","red"))} }
.oc-sdk-input[data-mono="true"] { font-family: ${_t}; }

.oc-sdk-search-icon { color: ${x}; }
.oc-sdk-search[data-active="true"] .oc-sdk-search-icon { color: ${D}; }
.oc-sdk-search-clear { border-radius: ${_o}; color: ${x}; }
.oc-sdk-search-clear:hover { background: ${w}; color: ${L}; }

.oc-sdk-trigger { border-color: transparent; border-radius: ${kt}; background: ${de}; color: ${oe}; ${Rt} transition: background 150ms ease-out; }
.oc-sdk-trigger:hover { background-image: linear-gradient(${w}, ${w}); }
.oc-sdk-trigger[aria-expanded="true"] { background-image: linear-gradient(${z}, ${z}); }
.oc-sdk-trigger-value[data-empty="true"] { color: ${x}; }
.oc-sdk-trigger-chevron { color: ${x}; }
.oc-sdk-popup { --surface-foreground: ${oe}; border-color: ${b(B,60)}; border-radius: 12px; background: ${de}; color: ${oe}; box-shadow: 0 8px 24px ${b(L,12)}; }
.oc-sdk-option { border-radius: 8px; }
.oc-sdk-option[data-active="true"] { background: ${w}; }
/* A selected option shows a check mark in the text colour, like the app's own pickers. Options without the mark come
   from bundles built before it existed and keep the selection fill as their only sign of the choice. */
.oc-sdk-option[aria-selected="true"]:not([data-mark]) { background: ${xt}; color: ${bt}; }
.oc-sdk-option-check { color: ${L}; }
.oc-sdk-option[data-destructive="true"] { color: ${vt}; }
.oc-sdk-option[data-destructive="true"][data-active="true"] { background: ${b(T("status-error","red"),10)}; }
.oc-sdk-option-hint { color: ${x}; }
.oc-sdk-popup-empty { color: ${x}; }

/* Checked stays neutral: the mark carries the state, and the primary colour stays with primary actions. */
.oc-sdk-check-box { border-color: ${B}; border-radius: 4px; color: ${L}; transition: border-color 150ms ease-out; }
.oc-sdk-check[aria-checked="true"] .oc-sdk-check-box { border-color: ${b(L,60,B)}; }
.oc-sdk-check-thumb { border-radius: 9999px; background: ${B}; transition: background 150ms ease-out; }
.oc-sdk-check-thumb::after { border-radius: 9999px; background: ${q}; transition: transform 150ms ease-out; }
.oc-sdk-check[aria-checked="true"] .oc-sdk-check-thumb { background: ${D}; }
.oc-sdk-check[aria-checked="true"] .oc-sdk-check-thumb::after { transform: translateX(16px); }
.oc-sdk-check:focus-visible { box-shadow: none; }
.oc-sdk-check:focus-visible .oc-sdk-check-box, .oc-sdk-check:focus-visible .oc-sdk-check-thumb { ${ko} }
.oc-sdk-check-desc { color: ${x}; }

.oc-sdk-tabs { border-radius: 10px; }
.oc-sdk-tabs[data-track="true"] { background: ${b(L,4)}; box-shadow: inset 0 1px 2px 0 rgb(0 0 0 / 0.16); }
.oc-sdk-tab { border-radius: 8px; color: ${x}; transition: color 150ms ease-out, background 150ms ease-out; }
.oc-sdk-tab:hover { color: ${L}; }
.oc-sdk-tab[aria-selected="true"] { color: ${bt}; background: ${xt}; border-color: ${B}; }
.oc-sdk-tabs[data-track="true"] .oc-sdk-tab[aria-selected="true"] { color: ${oe}; background: ${de}; border-color: transparent; ${Rt} }
.oc-sdk-tab-count { color: ${x}; }

.oc-sdk-badge { border-radius: 9999px; background: ${w}; color: ${x}; }
.oc-sdk-badge[data-tone] { color: var(--oc-sdk-tone-text, var(--oc-sdk-tone)); background: ${b("var(--oc-sdk-tone)",15)}; }

.oc-sdk-row { border-radius: 6px; transition: background 120ms ease-out; }
.oc-sdk-row:hover, .oc-sdk-row[data-active="true"] { background: ${w}; }
.oc-sdk-row[aria-selected="true"] { background: ${xt}; color: ${bt}; }
.oc-sdk-row-lead { font-family: ${_t}; color: ${x}; }
.oc-sdk-row-sub { color: ${x}; }
.oc-sdk-row-meta { color: ${x}; }
.oc-sdk-row[aria-selected="true"] .oc-sdk-row-lead, .oc-sdk-row[aria-selected="true"] .oc-sdk-row-sub, .oc-sdk-row[aria-selected="true"] .oc-sdk-row-meta { color: inherit; opacity: .75; }
.oc-sdk-list-empty { color: ${x}; }

.oc-sdk-empty-body { color: ${x}; }

.oc-sdk-spinner { color: ${x}; }
.oc-sdk-spinner-ring { border-color: ${B}; border-top-color: ${D}; border-radius: 9999px; }

.oc-sdk-banner { border-color: ${b("var(--oc-sdk-tone)",40)}; border-radius: 8px; background: ${b("var(--oc-sdk-tone)",10)}; }
.oc-sdk-banner-title { color: var(--oc-sdk-tone-text, var(--oc-sdk-tone)); }
.oc-sdk-banner-body { color: ${x}; }

.oc-sdk-separator { color: ${x}; }
.oc-sdk-separator::before, .oc-sdk-separator::after { background: ${b(B,40)}; }

.oc-sdk-progress-label { color: ${x}; }
.oc-sdk-progress-track { border-radius: 9999px; background: ${B}; }
.oc-sdk-progress-fill { border-radius: 9999px; background: var(--oc-sdk-tone, ${D}); }

.oc-sdk-text a { color: ${Tt}; text-decoration: underline; text-underline-offset: 2px; }
.oc-sdk-text img { border-radius: 8px; border: 1px solid ${b(B,60)}; }
`,C=`
${qe}
${jo}
${Go}`;var Co="oc-sdk-ui-style";var O=(e)=>{let s=document.getElementById(Co);if(s instanceof HTMLStyleElement){if(s.textContent!==e)s.textContent=e;return}ze(document);let r=document.createElement("style");r.id=Co,r.textContent=e,document.head.appendChild(r)},A=(e,s)=>{let r=document.createElement(e);if(s)r.className=s;return r},X=(e)=>{let s=A("button",e);return s.type="button",s},I=(e,s)=>{let r=s??"";if(e.textContent!==r)e.textContent=r};var Ko=()=>{let e=document.createElement("span");return e.className="oc-sdk-spinner-ring",e.setAttribute("aria-hidden","true"),e},V=(e,s)=>{O(C);let r=s,a=X("oc-sdk oc-sdk-btn"),c=Ko(),m=document.createElement("span");a.append(m),e.append(a);let y=()=>{if(a.dataset.variant=r.variant??"default",a.dataset.size=r.size??"default",a.disabled=Boolean(r.disabled)||Boolean(r.loading),a.dataset.loading=r.loading?"true":"false",a.setAttribute("aria-busy",r.loading?"true":"false"),r.loading&&c.parentNode!==a)a.prepend(c);else if(!r.loading&&c.parentNode===a)c.remove();I(m,r.label)},E=()=>{if(r.disabled||r.loading)return;r.onClick()};return a.addEventListener("click",E),y(),{update:(G)=>{r={...r,...G},y()},dispose:()=>{a.removeEventListener("click",E),a.remove()}}};var Ct=(e,s)=>{O(C);let r=s,a=A("div","oc-sdk oc-sdk-empty"),c=A("h2","oc-sdk-empty-title"),m=A("p","oc-sdk-empty-body"),y=A("div","oc-sdk-empty-action");a.append(c,m,y),e.append(a);let E=null,G=()=>{if(I(c,r.title),I(m,r.body),m.hidden=!r.body,y.hidden=!r.action,!r.action){E?.dispose(),E=null;return}let M={label:r.action.label,onClick:r.action.onClick};if(E)E.update(M);else E=V(y,{...M,variant:"outline",size:"sm"})};return G(),{update:(M)=>{r={...r,...M},G()},dispose:()=>{E?.dispose(),E=null,a.remove()}}};var es=/^(\s*[-*+] )\[( |x|X)\] ?(.*)$/,Oo=(e)=>e.split(`
`).map((s)=>{let r=es.exec(s);return r?{kind:"item",prefix:r[1]??"- ",done:r[2]!==" ",text:r[3]??""}:{kind:"text",text:s}}),Po=(e)=>e.map((s)=>s.kind==="text"?s.text:`${s.prefix}[${s.done?"x":" "}] ${s.text}`).join(`
`),Be=(e)=>{let s=2166136261;for(let r=0;r<e.length;r+=1)s^=e.charCodeAt(r),s=Math.imul(s,16777619);return`${e.length}:${(s>>>0).toString(16)}`},wo=(e,s)=>{let r=-1,a="- ";e.forEach((E,G)=>{if(E.kind!=="item")return;r=G,a=E.prefix});let c={kind:"item",prefix:a,done:!1,text:s};if(r>=0)return[...e.slice(0,r+1),c,...e.slice(r+1)];return[...e.at(-1)?.kind==="text"&&e.at(-1)?.text===""?e.slice(0,-1):[...e],c,{kind:"text",text:""}]};var K=yt(),j=document.querySelector("#root");if(!j)throw Error("Missing root");var Mo=document.createElement("style");Mo.textContent=`
  html, body { margin: 0; height: 100%; background: var(--oc-bg); color: var(--oc-fg); font-family: var(--oc-font); font-size: 13px; }
  #root { box-sizing: border-box; height: 100%; overflow: auto; padding: 16px 20px; }
  .note { margin: 2px 0; color: var(--oc-muted); white-space: pre-wrap; min-height: 1em; }
  .item { display: flex; align-items: center; gap: 8px; margin: 2px 0; }
  .item input[type="checkbox"] { width: 16px; height: 16px; margin: 0; accent-color: var(--oc-primary); }
  .item input[type="text"] { flex: 1; min-width: 0; border: 1px solid transparent; border-radius: var(--oc-radius); padding: 4px 6px;
    background: transparent; color: inherit; font: inherit; }
  .item input[type="text"]:hover { border-color: var(--oc-border); }
  .item input[type="text"]:focus { outline: none; border-color: var(--oc-focus); background: var(--oc-elevated); }
  .item.done input[type="text"] { color: var(--oc-muted); text-decoration: line-through; }
  .item .remove { visibility: hidden; }
  .item:hover .remove, .item:focus-within .remove { visibility: visible; }
  .actions { margin-top: 12px; }
`;document.head.append(Mo);K.onReady((e)=>ht(e,document.documentElement));var W=[],se=!1,Pt=_e(null),wt=()=>Po(W),De=()=>{K.reportFileChange(Pt.observe(Be(wt())))},Ot=()=>{if(j.replaceChildren(),!W.some((s)=>s.kind==="item")&&se){Ct(j,{title:"No items",body:"This checklist is empty."});return}if(W.forEach((s,r)=>{if(s.kind==="text"){let y=document.createElement("p");y.className="note",y.textContent=s.text,j.append(y);return}let a=document.createElement("div");a.className=s.done?"item done":"item";let c=document.createElement("input");c.type="checkbox",c.checked=s.done,c.disabled=se,c.addEventListener("change",()=>{s.done=c.checked,a.className=s.done?"item done":"item",De()});let m=document.createElement("input");if(m.type="text",m.value=s.text,m.readOnly=se,m.setAttribute("aria-label","Item"),m.addEventListener("input",()=>{s.text=m.value,De()}),a.append(c,m),!se){let y=document.createElement("span");y.className="remove",V(y,{label:"Remove",variant:"ghost",size:"xs",onClick:()=>{W=W.filter((E,G)=>G!==r),De(),Ot()}}),a.append(y)}j.append(a)}),se)return;let e=document.createElement("div");e.className="actions",V(e,{label:"Add item",variant:"secondary",size:"sm",onClick:()=>{W=wo(W,""),De(),Ot();let s=j.querySelectorAll('.item input[type="text"]');s.item(s.length-1)?.focus()}}),j.append(e)};K.onFileOpen((e)=>{if(e.encoding!=="text"){K.reportFileUnsupported();return}W=Oo(e.content),se=e.readOnly,Pt=_e(Be(wt())),Ot()});K.onFileSnapshot(()=>{let e=wt();return{content:e,version:Be(e)}});K.onFileSaved((e)=>{K.reportFileChange({dirty:Pt.markSaved(e),edited:!1})});})();
