/** Bounded file operations on the authenticated console DataChannel. Never MCP. */
import { SessionAuthError, sha256 } from './session-auth.js';
export const FILE_MAX_BYTES = 8 * 1024 * 1024, FILE_CHUNK_BYTES = 32768;
export type FileAction = {kind:'info'} | {kind:'upload_begin';path:string;size:number;sha256:string} | {kind:'upload_write';token:string;offset:number;data:string} | {kind:'upload_commit';token:string} | {kind:'download_begin';path:string} | {kind:'download_read';token:string;offset:number} | {kind:'cancel';token:string};
interface Canon { text(value:string):void; int(value:number):void }
function ensure(value:unknown,message:string):asserts value { if (!value) throw new SessionAuthError(message); }
function exact(value:unknown, fields:string[]):asserts value is Record<string,unknown> { ensure(value && typeof value==='object' && !Array.isArray(value),'Invalid file object'); const keys=Object.keys(value); ensure(keys.length===fields.length && keys.every(k=>fields.includes(k)),'Unknown or missing file fields'); }
function integer(value:unknown,max=FILE_MAX_BYTES):asserts value is number { ensure(typeof value==='number' && Number.isSafeInteger(value) && value>=0 && value<=max,'Invalid file integer'); }
function hex(value:unknown,length:number):asserts value is string { ensure(typeof value==='string' && new RegExp(`^[0-9a-f]{${length}}$`).test(value),'Invalid file digest or token'); }
export function validateFilename(path:unknown):asserts path is string { ensure(typeof path==='string' && new TextEncoder().encode(path).length>=1 && new TextEncoder().encode(path).length<=255 && !/[\0/\\]/.test(path) && path!=='.' && path!=='..' && !path.startsWith('.veynode-'),'Invalid single filename'); ensure(!/[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/u.test(path),'Invalid filename Unicode'); }
export function encodeChunk(bytes:Uint8Array):string { return btoa(Array.from(bytes,b=>String.fromCharCode(b)).join('')); }
export function decodeChunk(data:unknown,empty=false):Uint8Array<ArrayBuffer> { ensure(typeof data==='string' && data.length<=43692 && /^[A-Za-z0-9+/]*={0,2}$/.test(data),'Invalid file base64'); let decoded:string; try {decoded=atob(data);} catch {throw new SessionAuthError('Invalid file base64');} ensure(btoa(decoded)===data && decoded.length<=FILE_CHUNK_BYTES && (empty || decoded.length>0),'Invalid file chunk'); return Uint8Array.from(decoded,c=>c.charCodeAt(0)); }
export function fileActionFields(action:FileAction,c:Canon):void {
  ensure(action && typeof action==='object','Invalid file operation');
  switch(action.kind) {
    case 'info': exact(action,['kind']); c.text(action.kind); return;
    case 'upload_begin': exact(action,['kind','path','size','sha256']);validateFilename(action.path);integer(action.size);hex(action.sha256,64);c.text(action.kind);c.text(action.path);c.int(action.size);c.text(action.sha256);return;
    case 'download_begin':exact(action,['kind','path']);validateFilename(action.path);c.text(action.kind);c.text(action.path);return;
    case 'upload_write':exact(action,['kind','token','offset','data']);hex(action.token,32);integer(action.offset);ensure(action.offset+decodeChunk(action.data).length<=FILE_MAX_BYTES,'File chunk exceeds total bound');c.text(action.kind);c.text(action.token);c.int(action.offset);c.text(action.data);return;
    case 'download_read':exact(action,['kind','token','offset']);hex(action.token,32);integer(action.offset);c.text(action.kind);c.text(action.token);c.int(action.offset);return;
    case 'upload_commit':case 'cancel':exact(action,['kind','token']);hex(action.token,32);c.text(action.kind);c.text(action.token);return;
    default:throw new SessionAuthError('Unsupported file operation');
  }
}
export const FILE_ERROR_CODES = ['already_exists','not_found','invalid_request','integrity_error','changed','busy','unavailable','timeout','io_error','cleanup_failed','durability_unknown','permission_denied','unsupported','forbidden'] as const;
export function validateFileResult(result:Record<string,unknown>,action:FileAction):void {
  if(result.ok===false) {exact(result,['ok','error']);exact(result.error,['code']);ensure(FILE_ERROR_CODES.includes(result.error.code as typeof FILE_ERROR_CODES[number]),'Unknown file failure');return;}
  exact(result,['ok','value']);ensure(result.ok===true,'Invalid file reply');const v=result.value;ensure(v && typeof v==='object' && !Array.isArray(v),'Invalid file value');const value=v as Record<string,unknown>;
  switch(action.kind) {
    case 'info':exact(value,['max_file_bytes','chunk_bytes','idle_timeout_seconds','absolute_timeout_seconds','path_scope','ownership']);ensure(value.max_file_bytes===FILE_MAX_BYTES && value.chunk_bytes===FILE_CHUNK_BYTES && value.idle_timeout_seconds===60 && value.absolute_timeout_seconds===300,'Unsupported file policy');ensure(typeof value.path_scope==='string' && value.path_scope.length<=256 && typeof value.ownership==='string' && value.ownership.length<=256,'Invalid file policy');break;
    case 'upload_begin':case 'download_begin':exact(value,['token','size','sha256','chunk_bytes','idle_timeout_seconds']);hex(value.token,32);integer(value.size);hex(value.sha256,64);ensure(value.chunk_bytes===FILE_CHUNK_BYTES && value.idle_timeout_seconds===60,'Unsupported transfer policy');if(action.kind==='upload_begin')ensure(value.size===action.size && value.sha256===action.sha256,'Upload identity mismatch');break;
    case 'upload_write':exact(value,['offset']);integer(value.offset);ensure(value.offset===action.offset+decodeChunk(action.data).length,'Upload offset mismatch');break;
    case 'upload_commit':exact(value,['published','size','sha256']);ensure(value.published===true,'Upload publication unconfirmed');integer(value.size);hex(value.sha256,64);break;
    case 'download_read':exact(value,['data','offset','eof','sha256']);integer(value.offset);hex(value.sha256,64);ensure(typeof value.eof==='boolean','Invalid file EOF');ensure(value.offset===action.offset+decodeChunk(value.data,value.eof===true).length,'Download offset mismatch');break;
    case 'cancel':exact(value,['cancelled']);ensure(value.cancelled===true,'File cancellation unconfirmed');break;
  }
}
export class FileOperationError extends Error {cleanupUnconfirmed=false;constructor(readonly code:string){super(`File operation failed: ${code}`);}}
export type FileRequest=(action:FileAction,signal?:AbortSignal)=>Promise<Record<string,unknown>>;
/** One transfer, bounded memory, no automatic retry after uncertain publication.
 * Closing the owning session is responsible for cleanup if transport fails. */
export class SessionFiles {
  private busy=false;private stopped=false;private cancelRequested=false;private localAbort=new AbortController();
  constructor(private readonly request:FileRequest){}
  get active(){return this.busy;}
  cancel(){this.cancelRequested=true;this.localAbort.abort();}
  dispose(){this.stopped=true;this.cancel();}
  private async call(action:FileAction):Promise<Record<string,unknown>> {ensure(!this.stopped,'File session closed');const result=await this.request(action,AbortSignal.timeout(20000));ensure(!this.stopped,'File session closed');validateFileResult(result,action);if(result.ok===false)throw new FileOperationError((result.error as {code:string}).code);return result.value as Record<string,unknown>;}
  private begin(){ensure(!this.busy && !this.stopped,'File transfer unavailable');this.busy=true;this.cancelRequested=false;this.localAbort=new AbortController();}
  private check(){ensure(!this.stopped,'File session closed');if(this.cancelRequested)throw new FileOperationError('cancelled');}
  private local<T>(work:Promise<T>,discard?:(value:T)=>void):Promise<T> {
    const signal=this.localAbort.signal;
    return new Promise((resolve,reject)=>{
      let finished=false;
      const finish=(error?:Error,value?:T)=>{if(finished)return;finished=true;clearTimeout(timer);signal.removeEventListener('abort',abort);if(error)reject(error);else resolve(value as T);};
      const abort=()=>finish(new FileOperationError('cancelled'));
      const timer=setTimeout(()=>finish(new FileOperationError('timeout')),30000);
      signal.addEventListener('abort',abort,{once:true});
      work.then(value=>{if(finished)discard?.(value);else finish(undefined,value);},()=>finish(new FileOperationError('local_read_failed')));
      if(signal.aborted)abort();
    });
  }
  async upload(path:string,file:Blob):Promise<{size:number;sha256:string}> {
    validateFilename(path);integer(file.size);this.begin();let token:string|undefined;let bytes:Uint8Array<ArrayBuffer>|undefined;let primary:unknown;
    try {bytes=new Uint8Array(await this.local(file.arrayBuffer(),buffer=>new Uint8Array(buffer).fill(0)));this.check();const digest=await this.local(sha256(bytes));this.check();const begun=await this.call({kind:'upload_begin',path,size:bytes.length,sha256:digest});token=begun.token as string;
      for(let offset=0;offset<bytes.length;offset+=FILE_CHUNK_BYTES){this.check();await this.call({kind:'upload_write',token,offset,data:encodeChunk(bytes.subarray(offset,offset+FILE_CHUNK_BYTES))});}
      this.check();const result=await this.call({kind:'upload_commit',token});token=undefined;ensure(result.size===bytes.length && result.sha256===digest,'Published upload integrity mismatch');return {size:bytes.length,sha256:digest};
    } catch(error){primary=error;throw error;} finally {bytes?.fill(0);try {await this.clean(token,primary);}finally{this.busy=false;}}
  }
  private async clean(token:string|undefined,primary:unknown):Promise<void> {
    if(!token || this.stopped)return;
    try {await this.call({kind:'cancel',token});}
    catch(error){
      if(error instanceof FileOperationError && error.code==='not_found')return;
      this.stopped=true;
      if(primary instanceof FileOperationError){primary.cleanupUnconfirmed=true;return;}
      const failure=new FileOperationError('cleanup_unconfirmed');failure.cleanupUnconfirmed=true;throw failure;
    }
  }
  async download(path:string):Promise<{bytes:Uint8Array<ArrayBuffer>;sha256:string}> {
    validateFilename(path);this.begin();let token:string|undefined;let bytes:Uint8Array<ArrayBuffer>|undefined;let success=false;let primary:unknown;
    try {this.check();const begin=await this.call({kind:'download_begin',path});token=begin.token as string;bytes=new Uint8Array(begin.size as number);let offset=0;
      for(;;){this.check();const chunk=await this.call({kind:'download_read',token,offset});const data=decodeChunk(chunk.data,chunk.eof===true);ensure(chunk.sha256===begin.sha256 && offset+data.length<=bytes.length,'Download identity or length mismatch');bytes.set(data,offset);offset+=data.length;if(chunk.eof===true){token=undefined;ensure(offset===bytes.length,'Early download EOF');break;}ensure(offset<bytes.length,'Missing download EOF');}
      ensure(await this.local(sha256(bytes))===begin.sha256,'Downloaded file SHA-256 mismatch');this.check();success=true;return {bytes,sha256:begin.sha256 as string};
    }catch(error){primary=error;throw error;}finally{if(!success)bytes?.fill(0);try{await this.clean(token,primary);}finally{this.busy=false;}}
  }
}
