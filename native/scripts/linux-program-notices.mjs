import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

// @vscode/ripgrep 1.18.0 packages Microsoft's v15.0.1 prebuild of
// BurntSushi/ripgrep 15.0.0. The Microsoft wrapper keeps its LICENSE in npm.
// Sherpa's v1.13.8 cmake/onnxruntime-linux-{x86_64,aarch64}.cmake pin
// separate 1.28.2 archives; both carry the texts of the same ONNX commit.
const upstream = 'https://raw.githubusercontent.com';
const rg = `${upstream}/BurntSushi/ripgrep/3a612f88b805e14aef45bfa43e25a54abc6297fc`;
const sherpa = `${upstream}/k2-fsa/sherpa-onnx/11afbd009a7f8c08f4bcf2fc1b265d0df4670fbf`;
const onnx = `${upstream}/csukuangfj/onnxruntime/33ca9628233dc8f002435e868d4c2e9f82766ca1`;
// Both Linux 7za members of 7zip-bin 5.2.0 identify p7zip 16.02. These DOC
// files match the 16.02 source release; the npm package has its own MIT text.
const p7zip = `${upstream}/p7zip-project/p7zip/96c6249168b6125b523cad2dd3b01de24f527e9f/DOC`;
export const programNotices = [
  ['ripgrep/COPYING', `${rg}/COPYING`, '01c266bced4a434da0051174d6bee16a4c82cf634e2679b6155d40d75012390f'],
  ['ripgrep/LICENSE-MIT', `${rg}/LICENSE-MIT`, '0f96a83840e146e43c0ec96a22ec1f392e0680e6c1226e6f3ba87e0740af850f'],
  ['ripgrep/UNLICENSE', `${rg}/UNLICENSE`, '7e12e5df4bae12cb21581ba157ced20e1986a0508dd10d0e8a4ab9a4cf94e85c'],
  ['sherpa-onnx/LICENSE', `${sherpa}/LICENSE`, 'cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30'],
  ['onnxruntime/LICENSE', `${onnx}/LICENSE`, '2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c'],
  ['onnxruntime/ThirdPartyNotices.txt', `${onnx}/ThirdPartyNotices.txt`, '0e07b95f3a8d6230037707c5c4a2b554d12c4cb67369669ac255635528ffcee2'],
  ['p7zip/License.txt', `${p7zip}/License.txt`, '555806657dcf0f1e720b581c52643c195ec86ae3f00bd18cc66d2e0f88ffa210'],
  ['p7zip/copying.txt', `${p7zip}/copying.txt`, 'dc626520dcd53a22f727af3ee42c770e56c97a64fe3adb063799d8ab032fe551'],
];

const binaries = {
  x64: {
    rg: '193906679498de4d939345b937fa24e0e69a03c244bd70c859f5e41232713f21',
    onnx: '4b3607aebd1784b26b6f9b20e4bd974c7ab8287043e4d095cb7d2cb40b5e566e',
    p7zip: 'afc9448bd0cc2eeda131cce313ef4994f9656417e0a15c8465fcda9ca859b280',
  },
  arm64: {
    rg: 'e152ea689d6e8420357e592f0d8253b96476c164118ca3e6e13074fa1705ddda',
    onnx: 'cfc800d4438f15e8d9017521326776ca7117d8783cb847ccccc2fc61ac4d540f',
    p7zip: 'd363b0055afb4f7f336152dd5d00f1cc992d6e86a56e4a1ec84b2ac26dee27ff',
  },
};

export function checkBytes(name, bytes, expected) {
  if (createHash('sha256').update(bytes).digest('hex') !== expected) {
    throw new Error(`SHA-256 mismatch for ${name}; check the upstream program and its notices`);
  }
}

export async function stageProgramNotices(resources, arch, download) {
  const platform = `sherpa-onnx-linux-${arch}`;
  for (const [name, version] of [['sherpa-onnx-node', '1.13.8'], [platform, '1.13.8'], ['7zip-bin', '5.2.0']]) {
    const manifest = JSON.parse(fs.readFileSync(path.join(resources, 'node_modules', name, 'package.json'), 'utf8'));
    if (manifest.name !== name || manifest.version !== version) throw new Error(`No upstream notice mapping for ${manifest.name}@${manifest.version}`);
  }
  for (const [file, expected] of [
    ['ripgrep/rg', binaries[arch].rg],
    [`node_modules/${platform}/libonnxruntime.so`, binaries[arch].onnx],
    [`node_modules/7zip-bin/linux/${arch}/7za`, binaries[arch].p7zip],
    ['node_modules/7zip-bin/LICENSE.txt', 'e9f2248a07935f444c613d84e5a0239eab7537a80cc02212f386662f0ded0e11'],
  ]) checkBytes(file, fs.readFileSync(path.join(resources, file)), expected);

  for (const [file, url, sha256] of programNotices) {
    const bytes = Buffer.from(await (await download(url)).arrayBuffer());
    checkBytes(url, bytes, sha256);
    const destination = path.join(resources, 'notices', file);
    fs.mkdirSync(path.dirname(destination), { recursive: true });
    fs.writeFileSync(destination, bytes);
  }
}
