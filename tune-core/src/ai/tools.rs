use crate::ai::client::Tool;
use serde_json::json;

pub fn all_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "play_album".into(),
            description: "Search the local music library for an album by name and start playing it on the active zone.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "album_name": {
                        "type": "string",
                        "description": "Album name (or partial name) to search for"
                    }
                },
                "required": ["album_name"]
            }),
        },
        Tool {
            name: "play_track".into(),
            description: "Search the local music library for a track by name and start playing it on the active zone.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "track_name": {
                        "type": "string",
                        "description": "Track title (or partial title) to search for"
                    }
                },
                "required": ["track_name"]
            }),
        },
        Tool {
            name: "search_library".into(),
            description: "Search the local music library for artists, albums, and tracks matching a query. Returns results without playing anything.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query (artist name, album title, track title, genre, etc.)"
                    }
                },
                "required": ["query"]
            }),
        },
        Tool {
            name: "add_to_queue".into(),
            description: "Add a track to the current playback queue by its track ID.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "track_id": {
                        "type": "integer",
                        "description": "The numeric ID of the track to add to the queue"
                    }
                },
                "required": ["track_id"]
            }),
        },
        Tool {
            name: "pause".into(),
            description: "Pause the current playback on the active zone.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "resume".into(),
            description: "Resume playback on the active zone (unpause).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "set_volume".into(),
            description: "Set the playback volume on the active zone.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "volume": {
                        "type": "number",
                        "description": "Volume level between 0.0 (mute) and 1.0 (maximum)"
                    }
                },
                "required": ["volume"]
            }),
        },
        Tool {
            name: "next_track".into(),
            description: "Skip to the next track in the playback queue.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "now_playing".into(),
            description: "Get information about the track currently playing (title, artist, album, position, volume).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "list_zones".into(),
            description: "List all available playback zones (rooms/outputs).".into(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "set_zone".into(),
            description: "Switch the active zone for subsequent playback commands.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "zone_id": {
                        "type": "integer",
                        "description": "The numeric ID of the zone to activate"
                    }
                },
                "required": ["zone_id"]
            }),
        },
    ]
}
