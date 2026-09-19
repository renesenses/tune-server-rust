import test from 'node:test';import assert from 'node:assert/strict';
import {MessageChannel} from 'node:worker_threads';import {servePluginPort,mountPluginFrame} from './bridge.mjs';
test('iframe port scopes commands and releases spectrum subscription',async()=>{
 const c=new MessageChannel();let stop=0;let changed;
 const close=servePluginPort(c.port1,{configure:async x=>{changed=x;return {runtime_applied:false};},levels:()=>()=>stop++});
 const request=(id,method,args=[])=>new Promise(resolve=>{c.port2.once('message',resolve);c.port2.postMessage({id,method,args});});
 assert.equal((await request(1,'fetch',['/api/admin'])).error,'capability_missing');
 assert.deepEqual((await request(2,'configure',[{enabled:true}])).value,{runtime_applied:false});assert.deepEqual(changed,{enabled:true});
 await request(3,'subscribeLevels');await request(4,'subscribeLevels');close();assert.equal(stop,1);c.port2.close();
});
test('mount rejects arbitrary assets and messages from another window',()=>{
 let listener,removed=false;const frameWindow={postMessage(){throw Error('must not connect');}};
 const windowObject={location:{href:'https://tune.local/',origin:'https://tune.local'},addEventListener:(n,f)=>listener=f,removeEventListener:()=>removed=true};
 const iframe={contentWindow:frameWindow,setAttribute:(k,v)=>assert.equal(v,'allow-scripts')};
 assert.throws(()=>mountPluginFrame({iframe,assetUrl:'https://other.test/ui',client:{},windowObject}),/invalid_plugin_asset/);
 const close=mountPluginFrame({iframe,assetUrl:'/api/v1/audio-plugins/equalizer/assets/index.html',client:{},windowObject});listener({source:{},origin:'null',data:{type:'tune-plugin-ready'}});close();assert.ok(removed);assert.equal(iframe.src,'about:blank');
});
