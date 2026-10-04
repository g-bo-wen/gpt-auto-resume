// Original programmatic asset, no user image or network dependency.
import {writeFileSync,mkdirSync} from 'node:fs';
import {deflateSync} from 'node:zlib';
const size=64, raw=Buffer.alloc((size*4+1)*size);
for(let y=0;y<size;y++)for(let x=0;x<size;x++){
 const offset=y*(size*4+1)+1+x*4,dx=x-31.5,dy=y-31.5,r=Math.hypot(dx,dy);
 const ring=r>16&&r<21&&(Math.atan2(dy,dx)<2.7&&Math.atan2(dy,dx)>-2.2);
 const arrow=x>=12&&x<=25&&y>=14&&y<=25&&x+y<39;
 const a=(x<4||x>59)&&(y<4||y>59)?0:255;
 raw.set(ring||arrow?[245,255,250,a]:[23,107,88,a],offset);
}
function crc32(b){let c=0xffffffff;for(const x of b){c^=x;for(let i=0;i<8;i++)c=(c>>>1)^((c&1)?0xedb88320:0);}return (c^0xffffffff)>>>0;}
function chunk(name,data){const n=Buffer.from(name),len=Buffer.alloc(4),crc=Buffer.alloc(4);len.writeUInt32BE(data.length);crc.writeUInt32BE(crc32(Buffer.concat([n,data])));return Buffer.concat([len,n,data,crc]);}
const ihdr=Buffer.alloc(13);ihdr.writeUInt32BE(size);ihdr.writeUInt32BE(size,4);ihdr[8]=8;ihdr[9]=6;
const png=Buffer.concat([Buffer.from([137,80,78,71,13,10,26,10]),chunk('IHDR',ihdr),chunk('IDAT',deflateSync(raw)),chunk('IEND',Buffer.alloc(0))]);
const ico=Buffer.alloc(22);ico.writeUInt16LE(1,2);ico.writeUInt16LE(1,4);ico[6]=size;ico[7]=size;ico.writeUInt16LE(1,10);ico.writeUInt16LE(32,12);ico.writeUInt32LE(png.length,14);ico.writeUInt32LE(22,18);
mkdirSync('src-tauri/icons',{recursive:true});writeFileSync('src-tauri/icons/icon.ico',Buffer.concat([ico,png]));writeFileSync('src-tauri/icons/icon.png',png);
