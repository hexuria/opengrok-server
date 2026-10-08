
-- The screen tools (open_url, computer) joined the built-ins. A grant or ceiling written as
-- EXACTLY the previous built-in set was "everything this server implements" when it was written,
-- so it follows the built-ins; a narrower or wider list was chosen on purpose and is left alone.
-- Idempotent: once widened, the row no longer matches.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["read_file", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["read_file", "shell", "write_file"]}'::jsonb and not chosen;
-- `run_recipe` joined the built-ins the same way: a row that is exactly the five-tool set
-- follows; the two statements chain, so a three-tool row widens twice in one boot.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb and not chosen;
-- `request_user_form` joined the built-ins the same way: a row that is exactly today's
-- previous set follows; a narrower list was chosen on purpose and is left alone.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb and not chosen;
-- `credential.request` joined the built-ins the same way. Site passwords are NOT stored;
-- this tool only asks the client to fill a saved login.
update grant_view
   set profile = '{"only": ["computer", "credential.request", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
   and not exists (select 1 from ceiling_view c where c.coworker_id = grant_view.coworker_id and c.chosen);
update ceiling_view
   set tools = '{"only": ["computer", "credential.request", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb, version = version + 1
 where tools = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb and not chosen;
-- `credential.request` left with the broker (Sep 2026): the saved login is offered on the
-- ordinary form card. The widening just above still matches today's default grant, so it
-- would put the dead name on every fresh bot at every boot; this takes it out again.
update grant_view
   set profile = jsonb_set(profile, '{only}', (profile->'only') - 'credential.request')
 where profile->'only' ? 'credential.request';
update ceiling_view
   set tools = jsonb_set(tools, '{only}', (tools->'only') - 'credential.request'), version = version + 1
 where tools->'only' ? 'credential.request';
-- #268: a coworker's ceiling now decides whether it may reach its person's machine, and every
-- ceiling written before that allowed the machine in effect, so each gains it ONCE. LAST, after
-- the widenings above: they look for an exact older list, which never names the machine, so a
-- ceiling on an older list that gained it first would stay narrow for good while its profile
-- widened. After the pass a ceiling without the machine is one its owner switched off, which a
-- second pass would switch back on, so its row in schema_migrations makes every later boot's
-- pass match nothing. A row an older replica writes mid-deploy misses it: the machine is off.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from jsonb_array_elements_text((tools->'only') || '["user_machine_shell"]'::jsonb) tool))
 where tools ? 'only' and not (tools->'only' ? 'user_machine_shell')
   and not exists (select 1 from schema_migrations where name = 'the-machine-joins-the-ceiling');
insert into schema_migrations (name) values ('the-machine-joins-the-ceiling') on conflict do nothing;
-- #314: messaging the person's other Bots is a ceiling row, on by default, so every ceiling gains
-- it ONCE, after the machine and for the same reasons; one without it after this was switched off.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from jsonb_array_elements_text((tools->'only') || '["message_bot"]'::jsonb) tool))
 where tools ? 'only' and not (tools->'only' ? 'message_bot')
   and not exists (select 1 from schema_migrations where name = 'the-bots-join-the-ceiling');
insert into schema_migrations (name) values ('the-bots-join-the-ceiling') on conflict do nothing;
-- #316: a Bot's four routine tools are one ceiling row, on by default, so every list ceiling gains
-- them ONCE, after the machine and the Bots and for the same reasons; and its owner's profile too,
-- which the run's policy intersects with it (neither of those needed a profile). One without them
-- after this was switched off, and a set that admits nothing still admits nothing.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from (select distinct jsonb_array_elements_text((tools->'only') || '["create_routine",
           "delete_routine", "list_routines", "update_routine"]'::jsonb) tool) known))
 where not (tools->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}')
   and not exists (select 1 from schema_migrations where name = 'the-routines-join-the-ceiling');
update grant_view
   set profile = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from (select distinct jsonb_array_elements_text((profile->'only') || '["create_routine",
           "delete_routine", "list_routines", "update_routine"]'::jsonb) tool) known))
 where not (profile->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}')
   and not exists (select 1 from schema_migrations where name = 'the-routines-join-the-ceiling');
insert into schema_migrations (name) values ('the-routines-join-the-ceiling') on conflict do nothing;
-- #337: `run_routine` is the Routines row's fifth tool, so a list ceiling, and its owner's profile,
-- that has the row's four gains it ONCE, after them; one without them had the row switched off.
update ceiling_view
   set version = version + 1, tools = jsonb_build_object('only', (select jsonb_agg(tool order by
     tool collate "C") from jsonb_array_elements_text((tools->'only') || '["run_routine"]') tool))
 where tools->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}'
   and not (tools->'only' ? 'run_routine')
   and not exists (select 1 from schema_migrations where name = 'the-routine-runs-join-the-ceiling');
update grant_view
   set profile = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
     from jsonb_array_elements_text((profile->'only') || '["run_routine"]') tool))
 where profile->'only' ?& '{create_routine,delete_routine,list_routines,update_routine}'
   and not (profile->'only' ? 'run_routine')
   and not exists (select 1 from schema_migrations where name = 'the-routine-runs-join-the-ceiling');
insert into schema_migrations (name) values ('the-routine-runs-join-the-ceiling') on conflict do nothing;
-- #359: a Bot's plugin tools are one ceiling row, on by default like Routines, so every list
-- ceiling and its owner's profile gains them ONCE; one without them after this was switched off,
-- and a set that admits nothing still admits nothing.
update ceiling_view
   set version = version + 1,
       tools = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from (select distinct jsonb_array_elements_text((tools->'only') || '["add_plugin_account",
           "install_plugin", "list_plugin_accounts", "list_plugins", "pick_plugin_account",
           "plugin_details", "remove_plugin_account", "rename_plugin_account",
           "set_plugin_for_bot", "uninstall_plugin"]'::jsonb) tool) known))
 where tools ? 'only' and not (tools->'only' ?& '{add_plugin_account,install_plugin,list_plugin_accounts,list_plugins,pick_plugin_account,plugin_details,remove_plugin_account,rename_plugin_account,set_plugin_for_bot,uninstall_plugin}')
   and not exists (select 1 from schema_migrations where name = 'the-plugin-tools-join-the-ceiling');
update grant_view
   set profile = jsonb_build_object('only', (select jsonb_agg(tool order by tool collate "C")
         from (select distinct jsonb_array_elements_text((profile->'only') || '["add_plugin_account",
           "install_plugin", "list_plugin_accounts", "list_plugins", "pick_plugin_account",
           "plugin_details", "remove_plugin_account", "rename_plugin_account",
           "set_plugin_for_bot", "uninstall_plugin"]'::jsonb) tool) known))
 where profile ? 'only' and not (profile->'only' ?& '{add_plugin_account,install_plugin,list_plugin_accounts,list_plugins,pick_plugin_account,plugin_details,remove_plugin_account,rename_plugin_account,set_plugin_for_bot,uninstall_plugin}')
   and not exists (select 1 from schema_migrations where name = 'the-plugin-tools-join-the-ceiling');
insert into schema_migrations (name) values ('the-plugin-tools-join-the-ceiling') on conflict do nothing;
