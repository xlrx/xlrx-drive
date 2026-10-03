-- Does a space have results at all? (The search embeds a query only for spaces that do.)
CREATE INDEX ai_done_space ON ai_done (space);
