/** Framework-independent Tune SDK 0.1 UI client.
 * The host injects its authenticated fetch and existing event subscription;
 * credentials and Svelte stores never cross a plugin boundary. The UI cannot
 * grant itself zone/file access. Server authorization remains authoritative.
 */
export function createPluginClient({pluginId, zoneId, request, subscribe, locale="fr", theme="dark"}) {
  if (!['equalizer','crossfeed','converter','declick'].includes(pluginId)) throw new Error('unsupported_plugin');
  if (typeof request !== 'function' || typeof subscribe !== 'function') throw new TypeError('host_services_required');
  const zone = () => {
    if (!Number.isSafeInteger(zoneId) || zoneId < 1) throw new Error('zone_required');
    return `/api/v1/zones/${zoneId}`;
  };
  const batch = () => {
    if (!['converter','declick'].includes(pluginId)) throw new Error('batch_capability_required');
    return `/api/v1/${pluginId}`;
  };
  const id = value => {
    if (typeof value !== 'string' || !/^[a-zA-Z0-9-]{1,128}$/.test(value)) throw new Error('invalid_handle');
    return encodeURIComponent(value);
  };
  return Object.freeze({
    context() { return {protocol:{major:0,minor:1},plugin_id:pluginId,zone_id:zoneId,locale,theme}; },
    async configuration() {
      if (!['equalizer','crossfeed'].includes(pluginId)) throw new Error('dsp_capability_required');
      const result=await request(`${zone()}/dsp`, {method:'GET'});
      return result[pluginId==='equalizer'?'eq_profile':'crossfeed'];
    },
    async response({sampleRate=44100,channels=2}={}) {
      if(pluginId!=='equalizer' || !Number.isSafeInteger(sampleRate) || sampleRate<8000 || sampleRate>768000 || !Number.isSafeInteger(channels) || channels<1 || channels>32)throw new Error('invalid_response_format');
      return request(`${zone()}/eq/response?sample_rate=${sampleRate}&channels=${channels}`,{method:'GET'});
    },
    async configure(value) {
      if (!['equalizer','crossfeed'].includes(pluginId)) throw new Error('dsp_capability_required');
      const key = pluginId === 'equalizer' ? 'eq_profile' : 'crossfeed';
      return request(`${zone()}/dsp`, {method:'PUT', body:{[key]:value}});
    },
    async equalizer(path, {method='GET',body}={}) {
      if (pluginId !== 'equalizer' || !/^\/(?:status|presets(?:\/[a-zA-Z0-9_-]+(?:\/activate)?)?|import\/autoeq|bands|expert-settings)$/.test(path)) throw new Error('invalid_eq_operation');
      let query='';
      if(path.endsWith('/activate')){zone();query=`?zone_id=${zoneId}`;}
      if(method!=='GET' && body && (path==='/presets' || path==='/import/autoeq')){zone();body={...body,zone_id:String(zoneId)};}
      return request(`/api/v1/eq${path}${query}`, {method,body});
    },
    async capabilities() {
      if (pluginId === 'declick') return {formats:{flac:true,wav:true}};
      return request(`${batch()}/capabilities`, {method:'GET'});
    },
    async start(options) { return request(`${batch()}/start`, {method:'POST',body:options}); },
    async status(jobId) { return request(`${batch()}/status/${id(jobId)}`, {method:'GET'}); },
    async cancel(jobId) { return request(`${batch()}/jobs/${id(jobId)}`, {method:'DELETE'}); },
    downloadUrl(jobId) { return `${batch()}/download/${id(jobId)}`; },
    levels(callback) {
      zone(); let epoch = null, sequence = null, position = -1;
      return subscribe('playback.audio_levels', data => {
        if (data.zone_id !== zoneId || !Number.isSafeInteger(data.generation) || !Number.isSafeInteger(data.play_seq)) return;
        if (sequence !== null && (data.play_seq < sequence || (data.play_seq === sequence && data.generation < epoch))) return;
        if (!Number.isFinite(data.position_ms) || (data.play_seq === sequence && data.generation === epoch && data.position_ms < position)) return;
        const arrays=['spectrum','spectrum_db','spectrum_hz','spectrum_resolved'];
        if (arrays.some(k=>!Array.isArray(data[k])) || !data.spectrum.length || arrays.some(k=>data[k].length!==data.spectrum.length)) return;
        if (['spectrum','spectrum_db','spectrum_hz'].some(k=>data[k].some(v=>!Number.isFinite(v)))) return;
        if (data.observation_point !== 'decoded_source' || !(data.spectrum_resolution_hz > 0)) return;
        sequence=data.play_seq;epoch=data.generation;position=data.position_ms; callback(data);
      });
    },
  });
}
