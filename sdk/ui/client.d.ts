export type PluginId='equalizer'|'crossfeed'|'converter'|'declick';
export interface Spectrum { zone_id:number;play_seq:number;generation:number;position_ms:number;spectrum:number[];spectrum_db:number[];spectrum_hz:number[];spectrum_resolved:boolean[];spectrum_resolution_hz:number;observation_point:'decoded_source';provenance:string; }
export interface Host { pluginId:PluginId;zoneId?:number;locale?:string;theme?:string;request:(path:string,options:{method:string;body?:unknown})=>Promise<any>;subscribe:(event:'playback.audio_levels',callback:(value:Spectrum)=>void)=>()=>void; }
export type SourceSelection={track_id:number}|{album_id:number}|{path:string};
export interface ConverterRequest {sources:SourceSelection[];format:string;quality?:string;sample_rate?:number;bit_depth?:number;destination?:string;}
export interface DeclickRequest {sources:SourceSelection[];options?:{threshold_db?:number;trim_lead?:boolean;trim_tail?:boolean;zero_cross?:boolean;output_format?:'flac'|'wav'};}
export interface PluginClient {
 context():{protocol:{major:0;minor:1};plugin_id:PluginId;zone_id?:number;locale:string;theme:string};
 configuration():Promise<unknown>;
 configure(value:unknown):Promise<{runtime_applied?:boolean;[key:string]:unknown}>;
 response(options?:{sampleRate?:number;channels?:number}):Promise<{configuration_preview:true;response:{frequency_hz:number[];channels_db:number[][];sample_rate:number;provenance:'prepared_coefficients';includes_preamp:true}}>;
 equalizer(path:string,options?:{method?:string;body?:unknown}):Promise<unknown>;
 capabilities():Promise<unknown>;start(options:ConverterRequest|DeclickRequest):Promise<{job_id:string;total_tracks:number}>;
 status(jobId:string):Promise<unknown>;cancel(jobId:string):Promise<unknown>;downloadUrl(jobId:string):string;
 levels(callback:(spectrum:Spectrum)=>void):()=>void;
}
export function createPluginClient(host:Host):Readonly<PluginClient>;
