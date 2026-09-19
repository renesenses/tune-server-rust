/** Mount a sandboxed plugin UI. The caller supplies an already authorized
 * createPluginClient and a signed asset URL. No token, raw fetch, filesystem
 * path or arbitrary endpoint is exposed to the iframe. */
export function servePluginPort(port, client) {
  const allowed=new Set(['context','response','configuration','configure','equalizer','capabilities','start','status','cancel','downloadUrl']);
  let closed=false, stopLevels=null, inflight=0;
  port.onmessage=async ({data})=>{
    if(closed || !data || !Number.isSafeInteger(data.id) || !Array.isArray(data.args) || data.args.length>2) return;
    const reply=value=>{if(!closed)port.postMessage({id:data.id,...value});};
    if(inflight>=16){reply({error:'too_many_requests'});return;}
    inflight++;
    try{
      if(data.method==='subscribeLevels'){
        if(!stopLevels)stopLevels=client.levels(value=>{if(!closed)port.postMessage({event:'levels',value});});
        reply({value:true});
      }else if(data.method==='unsubscribeLevels'){
        stopLevels?.();stopLevels=null;reply({value:true});
      }else{
        if(!allowed.has(data.method))throw new Error('capability_missing');
        reply({value:await client[data.method](...data.args)});
      }
    }catch(e){reply({error:e instanceof Error?e.message:'host_failure'});}finally{inflight--;}
  };
  port.start?.();
  return()=>{closed=true;stopLevels?.();port.onmessage=null;port.close();};
}
export function mountPluginFrame({iframe,assetUrl,client,windowObject=window}) {
  const url=new URL(assetUrl,windowObject.location.href);
  if(url.origin!==windowObject.location.origin || !/^\/api\/v1\/audio-plugins\/(equalizer|crossfeed|converter|declick)\/assets\//.test(url.pathname))throw new Error('invalid_plugin_asset');
  iframe.setAttribute('sandbox','allow-scripts');
  let closePort=()=>{};
  const onReady=event=>{
    if(event.source!==iframe.contentWindow || event.origin!=='null' || event.data?.type!=='tune-plugin-ready')return;
    closePort();const channel=new MessageChannel();closePort=servePluginPort(channel.port1,client);
    // Opaque sandbox origins require '*'; the transferred capability is sent
    // solely to this iframe's WindowProxy, never broadcast to other frames.
    iframe.contentWindow.postMessage({type:'tune-plugin-connect',version:1},'*',[channel.port2]);
  };
  windowObject.addEventListener('message',onReady);iframe.src=url.href;
  return()=>{windowObject.removeEventListener('message',onReady);closePort();iframe.src='about:blank';};
}
export function connectPluginFrame(windowObject=window){
  return new Promise(resolve=>{
    const ready=event=>{
      if(event.source!==windowObject.parent || event.data?.type!=='tune-plugin-connect' || event.data.version!==1 || event.ports.length!==1)return;
      windowObject.removeEventListener('message',ready);
      const port=event.ports[0], pending=new Map();let next=1,levels=null,closed=false;
      port.onmessage=({data})=>{if(data?.event==='levels'){levels?.(data.value);return;}const entry=pending.get(data?.id);if(!entry)return;pending.delete(data.id);clearTimeout(entry.timer);data.error?entry.reject(new Error(data.error)):entry.resolve(data.value);};port.start?.();
      const call=(method,...args)=>new Promise((resolve,reject)=>{if(closed){reject(new Error('bridge_closed'));return;}const id=next++;const timer=setTimeout(()=>{pending.delete(id);reject(new Error('host_timeout'));},30000);pending.set(id,{resolve,reject,timer});port.postMessage({id,method,args});});
      resolve(Object.freeze({call,async levels(callback){levels=callback;await call('subscribeLevels');return()=>{levels=null;void call('unsubscribeLevels').catch(()=>{});};},close(){closed=true;levels=null;for(const e of pending.values()){clearTimeout(e.timer);e.reject(new Error('bridge_closed'));}pending.clear();port.close();}}));
    };windowObject.addEventListener('message',ready);windowObject.parent.postMessage({type:'tune-plugin-ready'},'*');
  });
}
