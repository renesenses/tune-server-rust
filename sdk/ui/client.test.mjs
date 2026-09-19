import test from 'node:test';
import assert from 'node:assert/strict';
import {createPluginClient} from './client.mjs';
test('spectrum filters zone, track, seek epoch and incomplete measurements without premium',()=>{
 let listener,closed=false;const received=[];
 const client=createPluginClient({pluginId:'equalizer',zoneId:7,request:()=>{},subscribe:(name,fn)=>{assert.equal(name,'playback.audio_levels');listener=fn;return()=>{closed=true;};}});
 const close=client.levels(x=>received.push(x));
 const data={zone_id:7,generation:1,play_seq:3,position_ms:100,spectrum:[0.8],spectrum_db:[-12],spectrum_hz:[1000],spectrum_resolved:[true],observation_point:'decoded_source',spectrum_resolution_hz:25};
 listener(data);listener({...data,zone_id:8});listener({...data,position_ms:0});listener({...data,generation:2,position_ms:0});listener(data);listener({...data,generation:3,spectrum:[]});listener({...data,generation:2,position_ms:20});
 assert.equal(received.length,3);assert.equal(received[1].generation,2);close();assert.ok(closed);
});
test('configuration changes only the owning feature and job handles cannot escape',async()=>{
 const calls=[];const host={zoneId:2,request:async(...args)=>{calls.push(args);return {runtime_applied:false};},subscribe:()=>()=>{}};
 const client=createPluginClient({...host,pluginId:'crossfeed'});const result=await client.configure({enabled:true});
 assert.deepEqual(calls[0],['/api/v1/zones/2/dsp',{method:'PUT',body:{crossfeed:{enabled:true}}}]);assert.equal(result.runtime_applied,false);
 const batch=createPluginClient({...host,pluginId:'converter'});assert.throws(()=>batch.downloadUrl('../other-job'),/invalid_handle/);assert.equal(batch.downloadUrl('abc-123'),'/api/v1/converter/download/abc-123');
});
test('EQ presets and AutoEq stay bound to the mounted zone',async()=>{
 const calls=[];const client=createPluginClient({pluginId:'equalizer',zoneId:9,request:async(...args)=>{calls.push(args);return {eq_profile:{enabled:true}};},subscribe:()=>()=>{}});
 assert.deepEqual(await client.configuration(),{enabled:true});
 await client.equalizer('/presets/global-id/activate',{method:'POST'});
 assert.equal(calls[1][0],'/api/v1/eq/presets/global-id/activate?zone_id=9');
 await client.equalizer('/import/autoeq',{method:'POST',body:{text:'Filter 1: ON PK Fc 1000 Hz Gain -3 dB Q 1',zone_id:'7'}});
 assert.equal(calls[2][1].body.zone_id,'9');
 await client.response({sampleRate:48000,channels:2});assert.equal(calls[3][0],'/api/v1/zones/9/eq/response?sample_rate=48000&channels=2');
});
