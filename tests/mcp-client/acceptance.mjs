// Real official MCP SDK client, not a handwritten wire-protocol simulator.
// stdio: node acceptance.mjs stdio STATE_DIR CONTROL_BIN [VM_ID]
// HTTPS: NODE_EXTRA_CA_CERTS=CA.pem node acceptance.mjs https URL TOKEN_FILE [VM_ID]
import {readFileSync} from 'node:fs';
import assert from 'node:assert/strict';
import {Client} from '@modelcontextprotocol/sdk/client/index.js';
import {StdioClientTransport} from '@modelcontextprotocol/sdk/client/stdio.js';
import {StreamableHTTPClientTransport} from '@modelcontextprotocol/sdk/client/streamableHttp.js';
const [mode,location,credential,vm]=process.argv.slice(2);
const transport=mode==='stdio'
  ? new StdioClientTransport({command:credential,args:['mcp','--state-dir',location,'--allow-execute']})
  : new StreamableHTTPClientTransport(new URL(location),{requestInit:{headers:{Authorization:`Bearer ${readFileSync(credential,'utf8').trim()}`}}});
const client=new Client({name:'veynode-official-sdk-acceptance',version:'1.0.0'},{capabilities:{}});
try {
  await client.connect(transport);
  const tools=await client.listTools();
  assert(tools.tools.some(t=>t.name==='vm_list'));
  const list=await client.callTool({name:'vm_list',arguments:{}});
  assert(!list.isError,JSON.stringify(list));
  const checks=['initialize','tools/list','vm_list'];
  if(vm){
    let guestInfo;
    for(const [name,args] of [['vm_guest_info',{id:vm}],['vm_acquire_control',{id:vm}],['vm_exec',{id:vm,argv:['/bin/uname','-m'],timeout_ms:5000}]]){
      const result=await client.callTool({name,arguments:args});
      assert(!result.isError,JSON.stringify(result));
      if(name==='vm_guest_info') guestInfo=JSON.parse(result.content[0].text).result;
      if(name==='vm_exec') assert.equal(JSON.parse(result.content[0].text).result.stdout,'aarch64\n');
      checks.push(name);
    }
    if(guestInfo.capabilities.async_exec){
      const call=async(name,args)=>{
        const response=await client.callTool({name,arguments:{id:vm,...args}});
        assert(!response.isError,JSON.stringify(response));
        return JSON.parse(response.content[0].text).result;
      };
      const job=await call('vm_exec_start',{argv:['/bin/sleep','10'],timeout_ms:15000});
      assert.equal(job.state,'running');
      const cancelled=await call('vm_exec_cancel',{job_id:job.job_id});
      assert.equal(cancelled.cancel_requested,true);
      let finished;
      for(let i=0;i<100;i++){
        const current=await call('vm_exec_poll',{job_id:job.job_id});
        if(current.state==='finished'){finished=current;break;}
        await new Promise(resolve=>setTimeout(resolve,50));
      }
      assert(finished,'Guest job did not actually finish');
      assert(!finished.error,JSON.stringify(finished));
      assert.equal(finished.result.cancelled,true);
      assert.equal(finished.result.timed_out,false);
      checks.push('vm_exec_start','vm_exec_cancel','vm_exec_poll actual cancelled completion');
    }
    const release=await client.callTool({name:'vm_release_control',arguments:{id:vm}});
    assert(!release.isError);checks.push('vm_release_control');
  }
  if(mode==='https'){await transport.terminateSession();checks.push('DELETE session');}
  console.log(JSON.stringify({passed:true,client:'official @modelcontextprotocol/sdk 1.32.1',transport:mode,checks}));
}finally{await client.close();}
