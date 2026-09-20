-- READING-STATE SYNC — the server half. Run this ONCE in a new Supabase project.
--
-- WHERE. Supabase dashboard → SQL Editor → New query → paste this whole file → Run. It is idempotent
-- enough to re-run: the table is created only if absent and each policy is dropped before it is made.
--
-- WHAT IT CREATES. One table holding one row per (reader, book), where the book is identified by the
-- SHA-256 of the file it came from — the same id on every device, so nothing has to be allocated and no
-- book content is ever uploaded. The `doc` column is the reading state itself.
--
-- WHY THE POLICIES ARE THE POINT. The publishable key ships inside the application and is not a secret.
-- What keeps one reader's rows away from another is `auth.uid() = user_id`, enforced by the database on
-- every read and every write. An application bug that forgets to filter cannot leak a library: the
-- database refuses the rows before the application sees them.
--
-- WHAT THIS DELIBERATELY DOES NOT HAVE. No delete policy: the application has no way to remove a
-- reader's synced state, so none is granted. A reader who wants it gone can delete the project's rows
-- from this dashboard, which is the honest place for a destructive act.

create table if not exists public.book_state (
  -- The account this row belongs to. Sent explicitly by the client (never defaulted) so that a row can
  -- never be filed under a null identity by mistake.
  user_id    uuid        not null references auth.users (id) on delete cascade,
  -- The book, by the SHA-256 of its file. Text, not uuid: it is a hash, and it is the same one Sard
  -- already uses as `books.id`.
  book_id    text        not null,
  -- Monotonic per row. The client sends the version it read; a write that does not match updates
  -- nothing, which is how two devices racing over one book are kept from overwriting each other.
  version    bigint      not null default 1,
  -- The reading state: position, the grow-only section sets, and the furthest mark.
  doc        jsonb       not null,
  updated_at timestamptz not null default now(),
  primary key (user_id, book_id)
);

alter table public.book_state enable row level security;

-- Nothing is reachable without a signed-in account, and never through the anonymous role.
revoke all on table public.book_state from anon, authenticated;
grant select, insert, update on table public.book_state to authenticated;

drop policy if exists "read own reading state" on public.book_state;
create policy "read own reading state" on public.book_state
  for select to authenticated
  using ((select auth.uid()) = user_id);

drop policy if exists "create own reading state" on public.book_state;
create policy "create own reading state" on public.book_state
  for insert to authenticated
  with check ((select auth.uid()) = user_id);

drop policy if exists "update own reading state" on public.book_state;
create policy "update own reading state" on public.book_state
  for update to authenticated
  using ((select auth.uid()) = user_id)
  with check ((select auth.uid()) = user_id);
