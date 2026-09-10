-- The Dolt image creates `root@localhost` only, so a connection arriving
-- through Docker's port map is denied — it is not localhost. This runs from the
-- entrypoint's init hook and grants a user that can actually be reached.
CREATE USER IF NOT EXISTS 'bench'@'%' IDENTIFIED BY 'bench';
GRANT ALL PRIVILEGES ON *.* TO 'bench'@'%' WITH GRANT OPTION;
