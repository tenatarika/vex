CREATE OR REPLACE FUNCTION greet(name TEXT) RETURNS TEXT AS $$
BEGIN
  RETURN 'hi ' || name;
END;
$$ LANGUAGE plpgsql;

CREATE TABLE notARelevantMatch (id INT);
